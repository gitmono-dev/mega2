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

## ObjectNamespace / object key

`ObjectNamespace::Agent` 的 `Display` / `to_string()` 为 `agent`。ObjectKey **path** 不再重复 namespace 段；后端完整路径为 `agent/<path>`，且与 Media 一样不按 3-level hash 分片。

| 阶段 | ObjectKey path |
|---|---|
| staging | `{deployment_id}/{tenant_id}/staging/{lease_id}` |
| committed | `{deployment_id}/{tenant_id}/{visibility}/sha256/{hex}` |

首版 `visibility=raw` 为事实源。`deployment_id` / `tenant_id` 来自 `[agent_capture]` 配置。客户端不得指定 final object key；finalize 从 staging 对象 `get_stream` 增量计算 sha256，再 `get_stream` 一次流式写入 committed key，忽略请求中的 final key。服务层不把整 blob 再缓冲成 `Vec<u8>`。`agent_capture_blob.object_key` 只存服务器派生值。

## HTTP path / error / pagination

Router 内部 path 以 `/agent-capture` 开头，由外层 nest 到 `/api/v1`。下表为完整外部路径；实现不得再加一层 `/api/v1`。本卡只登记 route-construction fixture（501），业务 handler 由后续卡替换、不改 path。挂载门见下文。

认证头：`Authorization: Bearer <ingest_token>`。查找失败（缺头、非 Bearer、空 secret、或 ingest token 未命中）对齐 401，`error.code` 为 `unauthorized`。token 不覆盖正规化 repo path 时为 404（后续卡）。判定顺序 401 → 404 → 409 → 400/413。

错误响应固定：

```json
{ "error": { "code": "unauthorized", "message": "invalid ingest token" } }
```

不得包含 token secret、raw prompt、tool 原文、或其他 tenant 是否存在的线索。

分页：query `limit` / `cursor`；缺省 `limit=50`，最大 `200`。list 响应 `{ "items": [...], "next_cursor": string|null }`。

`{repo}` 为单一 percent-encoded URL segment（禁止 Axum `{*repo}`）。解码后 `normalize_repo_path` 得到 `repo_id` TEXT。canonical fingerprint 调用 `crate::common::canonical_json`；PUT fingerprint 不含服务端 completeness/lifecycle。

| Method | Public path |
|---|---|
| GET | `/api/v1/agent-capture/discovery` |
| PUT | `/api/v1/agent-capture/repos/{repo}/sessions/{client_session_id}` |
| POST | `/api/v1/agent-capture/sessions/{capture_id}/blobs/staging` |
| POST | `/api/v1/agent-capture/sessions/{capture_id}/blobs/{lease_id}/finalize` |
| POST | `/api/v1/agent-capture/sessions/{capture_id}/events:batch` |
| POST | `/api/v1/agent-capture/sessions/{capture_id}/file-ops:batch` |
| POST | `/api/v1/agent-capture/sessions/{capture_id}/checkpoints` |
| GET | `/api/v1/agent-capture/repos/{repo}/sessions` |
| GET | `/api/v1/agent-capture/sessions/{capture_id}` |
| GET | `/api/v1/agent-capture/sessions/{capture_id}/checkpoints` |
| GET | `/api/v1/agent-capture/sessions/{capture_id}/transcript` |
| GET | `/api/v1/agent-capture/sessions/{capture_id}/file-ops` |

session JSON 字段：`capture_id`、`client_session_id`、`tenant_id`、`deployment_id`、`repo_id`、`producer_id`、`session_kind`（`external_capture` \| `internal_code`）、`started_at`、`ended_at`、`completeness`（`empty` \| `incomplete` \| `complete` \| `truncated`）、`partial_reason`、`created_at`、`updated_at`。无 `user_id`。identity 与服务端字段不可由客户端覆写；未知 JSON 字段拒绝。

## 挂载

`http_server` 仅当 `git.storage_only()` **且** `[agent_capture].enabled=true` 把 `agent_capture_router` nest 进已有 `/api/v1`。review 形态（`push_auth` 缺省）不 merge 该 router；`enabled=false` 的 storage-only 也不注册。未挂载时对 `/api/v1/agent-capture/discovery` 为裸 404，OpenAPI 不含 `/api/v1/agent-capture`。

`storage_only_openapi_doc(include_oci, include_agent_capture)` 与 `trunk_openapi_doc(include_oci, include_agent_capture)` 平行 `include_oci`：`include_agent_capture=false` 时路径字符串均不含 `/api/v1/agent-capture`。运行时挂载由配置门决定，不单独暴露该 flag 给运维。

## Discovery

`GET /api/v1/agent-capture/discovery` 需要 `Authorization: Bearer <ingest_token>`。命中 `lookup_ingest_token` 时返回 `{ "raw_accepted": true }`。缺头、非 Bearer、或 secret 未命中一律 401，`error.code` 为 `unauthorized`；响应不含 token secret。discovery 不访问数据库。

## Session PUT

`PUT /api/v1/agent-capture/repos/{repo}/sessions/{client_session_id}`：`{repo}` 单一 percent-encoded segment，解码一次后 `normalize_repo_path` 得到 `repo_id`。`producer_id` 为 ingest token `name`。无 token → 401（先于 path/body 解析）；token 不覆盖正规化 path → 404（先于读 body）。PUT fingerprint 绑定自然键 + immutable metadata（`session_kind` / `started_at` / `ended_at`），不含 completeness/lifecycle，也不随 `Idempotency-Key` 分叉；同自然键同 fingerprint 重试返回同一 `capture_id`（即使 completeness 已变）；不同 fingerprint → 409。响应 `{ "capture_id": i64 }`。

## Blob staging / finalize

`POST /api/v1/agent-capture/sessions/{capture_id}/blobs/staging` 与 `POST /api/v1/agent-capture/sessions/{capture_id}/blobs/{lease_id}/finalize` 以 session 为边界，无 repo-level staging。无 token → 401；未知 `capture_id` 或 token 不覆盖该 session 的 `repo_id` → 404。staging body 超过 `[agent_capture].max_blob_bytes`（含 chunked）→ 413。合法 staging 返回非空 `lease_id`。finalize 由服务器从 staging 对象重算 sha256；声明 `digest` 必须等于服务器 digest，否则 400。请求中的客户端 `object_key` 被忽略，响应 `object_key` 为服务器 committed key（`{deployment_id}/{tenant_id}/raw/sha256/{hex}`）。他人未过期 lease 的 finalize → 409。tenant/deployment 来自配置，不接受客户端覆写。

## CAS / fencing

lease 状态 `staging → finalizing → committed`（或 `expired`/`aborted`）。条件提交必须带 owner `lease_generation`；0 行更新视为 409，不得覆盖他方未过期 lease。同 lease 同 digest 重试返回既有 committed receipt。object 写入前先写 `upload_intent`；写入失败保留 intent 且不得报成功。staging 删除前写 `cleanup_intent`，删除失败保留 intent。`(capture_id, stream_kind, generation)` 绑定 raw blob 与 `blob_ref` 同事务；同水位同 digest 幂等，同水位不同 digest 409；过期 generation 的替换 409。

## Events batch

`POST /api/v1/agent-capture/sessions/{capture_id}/events:batch` 整批事务。`event_uid` 必须是 ASCII `{generation}:{byte_offset}`（非负十进制，不得溢出），否则 400。未知 `event_kind` 存 `unknown` 并保留 envelope payload。body `batch_id` 为幂等键；同 fingerprint 200，不同 409。同 uid 不同 payload 409。可选 `completeness` 为 `incomplete` 或 `complete`，只升不降（不把 `complete`/`truncated` 降级）。超过 `max_events_per_batch` 或单 event 超过 `max_event_bytes` → 413。无 token → 401。

## File-ops batch

`POST /api/v1/agent-capture/sessions/{capture_id}/file-ops:batch` 校验 `agent.file_op.v1`（`schema` 缺省即该值；其它 schema 400）。无 `source_event_uid`、空 uid、或同 capture 无对应 event → 400。`op ∈ {read, write, patch, delete, search}`；`op=search` 合法。路径为空、含 `..`、含 NUL、或绝对路径 → 400。digest 非空必须指向同 tenant/deployment 已 committed 的 raw blob，并写入 `blob_ref.owner_file_op_id`；未 committed → 400。body `batch_id` 为幂等键；receipt fingerprint 为整段 body 的 canonical JSON；同 fingerprint 200，不同 409。判定顺序 401 → 404 → 409（已有 receipt，先于 typed JSON 校验）→ 400。storage 错误映射 409 仅匹配固定前缀 `ingest receipt fingerprint conflict`。file-op 绑定的 blob 数 **超过** `max_file_blobs_per_session` 时 session `completeness` 标 `truncated`（等于上限不标）；超限仍 200 并写入。整批事务。无 token → 401。

## Checkpoint POST

`POST /api/v1/agent-capture/sessions/{capture_id}/checkpoints` 仅接受父 session `session_kind=external_capture`（`internal_code` → 400）。`transcript_digest` 必须指向同 tenant/deployment 已 committed 的 raw blob；仍为 staging 或缺失 → 400。`redacted_digest` 可选，写入 checkpoint `metadata`；仅 redacted、无 raw 时响应与 session 的 `completeness=incomplete`、`partial_reason=missing_raw`。同 `transcript_digest` 共享已 committed CAS blob 行，并写入 `blob_ref.owner_checkpoint_id`。body `checkpoint_id` 为幂等键；receipt fingerprint 为整段 body 的 canonical JSON；同 fingerprint 200，不同 409。判定顺序 401 → 404 → 409（已有 receipt，先于 typed JSON 校验）→ 400。无 token → 401。
