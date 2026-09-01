# FastCDC Media

> 状态：FC-01～FC-05 已定义 feature-gated v1 chunker/manifest contract、私有 Media scope 和 pending 上传会话；finalize/fallback 与 HTTP API 仍由 FC-06～FC-07 补齐。默认构建不启用该能力。

## v1 manifest

启用 Cargo feature `fastcdc` 后，Media manifest 使用固定的 JSON 字段：

| 字段 | 约束 |
|---|---|
| `version` | 必须为数字 `1` |
| `algorithm` | 必须为 `fastcdc-v1` |
| `hash_algorithm` | 必须为 `sha256` |
| `media_oid` | 64 位小写 SHA-256 hex |
| `media_size` | 所有 chunk 原始长度之和 |
| `chunks` | 至多 8192 项，连续、非零，单项至多 8 MiB |
| `chunks[].chunk_hash` | 该原始 chunk 的 64 位小写 SHA-256 hex |
| `chunks[].compression` | 固定为 `none` |
| `chunks[].encoded_length` | 必须等于 `length` |
| `chunks[].checksum` | v1 不接受；字段仅为 JSON 形状兼容保留 |
| `created_by` | 客户端 provenance，不参与 canonical ID |
| `fallback_oid` | 缺省或等于 `media_oid`，不参与 canonical ID |

原始 manifest JSON 最大为 10 MiB。解析与 canonical ID 计算前均须先执行校验；无效 version、算法、哈希、边界、大小或 fallback 必须返回 domain error，不能 panic。

## Canonical ID

`MediaManifest::id()` 对 `(version, algorithm, hash_algorithm, media_oid, media_size, chunks)` 的固定 JSON 序列化结果取 SHA-256 小写 hex。`created_by` 与等于 `media_oid` 的 `fallback_oid` 是非语义字段，因此变更它们不会改变 ID。

这个规则与 Mega `bb3ef17` 的 v1 contract 对齐。修改字段集合、序列化顺序、分块算法或任何 chunk 参数都需要新的 algorithm/version，不能原地改变 v1。

## Scope and storage keys

Media scope 只由服务端已验证的 Mono access-token 身份和 canonical repository 生成。当前 access-token 解析会填充 `LoginUser.username`，因此 FC-03 使用它作为 actor key；它不是 request JSON 的 actor/repository，也不读取 manifest 的 `created_by`。当前 access-token 路径没有 `website_user_id`：若 username 日后改名，会得到一个新 scope；任何旧 Media 对象迁移或 identity backfill 都必须走后续独立计划，不能静默修改 v1 digest。

repository 必须为绝对路径，且拒绝 backslash、`%`、`?`、空段、`.` 和 `..`。scope digest 是 `SHA-256(serde_json([actor, repository]))` 的小写 hex，因而 actor 和 repository 任一变化都会产生隔离的 key space。

逻辑对象 key 使用 `ObjectNamespace::Media`（稳定前缀 `media`）和 `media-v1/<scope-digest>/`：`pending/<manifest-id>`、`chunks/<chunk-hash>`、`finalized/<media-oid>`。底层仍应用 `ObjectKey` 的固定 sharding；它不会将 Media 与既有 LFS、Attachment 或其他 namespace 混合。scope 和原始 `ObjectKey` 构造 API 仅在 crate 内可见，且 scope 的格式化和错误不携带 actor、repository、digest 或内部 object key；HTTP adapter 在 FC-07 继续把存储失败映射为不泄漏这些值的公共错误。

## Pending upload lifecycle

`prepare` 只接受已通过 v1 校验的 manifest。服务端会将 `fallback_oid` 固定为
`media_oid`，再以 canonical manifest ID 写入当前 scope 的 `pending/<manifest-id>`。
pending payload（包括会话包装字段）最大为 10 MiB；其 `created_at` 采用 24 小时
逻辑 TTL，达到 TTL 的会话视为不存在，不能继续上传或读取。

prepare 仅检查当前 scope 中 manifest 声明的 chunk key。响应中的
`missing_chunks` 保持 manifest 首次出现的顺序，并对重复 hash 去重。上传必须先命中
仍有效的 pending manifest 中完全一致的 hash 和 length，再校验实际 SHA-256；同一
scope 中已有且校验正确的 chunk 直接复用，错误的上传内容会被拒绝。每个 chunk 至多
8 MiB，服务读取也保持该上限。

finalize 之前，服务只提供“当前 scope、当前 pending manifest、已声明 hash”的 chunk
读取能力；即使同 scope 下存在其他对象，也不能通过任意 hash 读取。此阶段返回的
Invalid、NotFound、Conflict、Storage、Io 和 Json 均为不携带 scope/key 的领域错误。
finalized manifest、标准 LFS fallback 和公开 HTTP 路由尚未实现。

## Responses and capabilities

prepare 响应使用 `manifest_id` 和 `missing_chunks`；已发布 manifest 响应使用 `manifest_id` 和 `manifest`。v1 capability payload 固定声明：

- `version: "1"`、`chunked_lfs: true`、`chunk_algorithms: ["fastcdc-v1"]`、`hash_algorithms: ["sha256"]`；
- `max_chunk_size: 8388608`、`max_manifest_size: 10485760`；
- `supports_batch_exists: true`、`supports_range_read: false`、`supports_standard_lfs_fallback: true`；
- `scope: "authenticated-user-and-repository"`。

此文档尚不声明公开 route；该协议面在对应实现可用后追加，避免向 feature-off 用户暴露未实现的接口。
