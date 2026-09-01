# FastCDC Media

> 状态：FC-01～FC-07 已定义 feature-gated v1 chunker/manifest contract、私有 Media scope、pending 上传会话、标准 LFS fallback 和 HTTP API；默认构建不启用该能力，最终 family release 仍由 FC-14 收口。

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

## Finalize and fallback

`finalize` 最多允许两个并发 finalizer。它只读取当前 scope、当前未过期
pending manifest 中声明的 chunks，并对每个 chunk 的 length 和 SHA-256 重新校验。服务
将内容重建到匿名临时文件，先验证完整 `media_oid`/`media_size`，再重跑冻结的
`fastcdc-v1`，要求每个 offset、length 和 hash 均与 manifest 一致；任一步失败都不会
发布标准 LFS fallback 或 finalized manifest，临时文件会在成功和失败时自动清理。

验证完成后，fallback 使用 `put_stream_bounded` 写入标准 `Lfs` namespace，并以
`ON CONFLICT DO NOTHING` 语义登记 `lfs_objects`，随后重新读取 metadata 验证 size 与
存在状态。只有该验证成功，才写入当前 scope 的 `finalized/<media-oid>` manifest。相同
canonical manifest 的重复 finalize 可安全重试；同一 media OID 的不同 finalized
manifest、错误 OID 的已存 manifest 或不同 scope 均会被拒绝。

## Responses and capabilities

prepare 响应使用 `manifest_id` 和 `missing_chunks`；已发布 manifest 响应使用 `manifest_id` 和 `manifest`。v1 capability payload 固定声明：

- `version: "1"`、`chunked_lfs: true`、`chunk_algorithms: ["fastcdc-v1"]`、`hash_algorithms: ["sha256"]`；
- `max_chunk_size: 8388608`、`max_manifest_size: 10485760`；
- `supports_batch_exists: true`、`supports_range_read: false`、`supports_standard_lfs_fallback: true`；
- `scope: "authenticated-user-and-repository"`。

## HTTP API

启用 Cargo feature `fastcdc` 时，客户端必须在已有 repository LFS URL 后追加
`/libra/media/v1`。因此服务端实际可调用前缀为：

```
<canonical-repository>/info/lfs/libra/media/v1
```

例如 repository 为 `/project/demo.git` 时，capabilities URL 是
`/project/demo.git/info/lfs/libra/media/v1/capabilities`。HTTP server 会在标准 LFS URI
改写前保存这个原始 repository 前缀；Media scope 仅从它和已验证的 access-token username
构造。`/api/openapi.json` 将接口列为 `/info/lfs/libra/media/v1/...`，并在每个 Media path
上声明 OpenAPI server variable `/{repository}`（默认 `project/demo.git`）；组合后就是实际
可调用的 repository-scoped URL。不会注册或文档化 repository-free 的
`/api/v1/lfs/libra/media/v1/...` alias。

所有端点都要求 `Authorization: Bearer <mono-access-token>`，不会接受普通 LFS 的 Basic
凭据作为 Media 身份。`/api/openapi.json` 的每个 Media operation 都引用
`monoAccessToken` HTTP Bearer security scheme，供生成客户端发现这一要求。相对上述 Media
前缀的路由为：

| 方法 | 路径 | 结果 |
|---|---|---|
| `GET` | `/capabilities` | 返回固定 v1 capabilities。 |
| `POST` | `/manifests` | 校验并创建/恢复 pending manifest，返回 manifest ID 和缺失 hash。 |
| `PUT` | `/manifests/{manifest_id}/chunks/{hash}` | 校验并写入一个声明的 chunk。 |
| `POST` | `/manifests/{manifest_id}/finalize` | 完整校验后发布标准 LFS fallback。 |
| `GET` | `/manifests/by-media/{media_oid}` | 返回当前 scope 的 finalized manifest。 |
| `GET` | `/manifests/by-media/{media_oid}/chunks/{hash}` | 仅从 finalized manifest 中读取已声明且再次校验的 chunk。 |

manifest route 由请求层限制为 10 MiB，chunk route 限制为 8 MiB；带超限
`Content-Length` 的请求会在 handler 前以 `413` 拒绝，未知长度的流也受同一上限约束。
Media 领域错误映射为 Invalid=`400`、NotFound=`404`、Conflict=`409` 和
Storage/Io/Json=`500`。所有 `500` 响应固定为 `media storage operation failed`，不返回
scope digest、repository 或 object key。普通 LFS route 不变；未启用 feature 时 Media route
和其 OpenAPI paths 均不会注册。本卡不新增 `MegaError` 变体或 TOML 配置项：启用面仅由
Cargo feature `fastcdc` 控制。
