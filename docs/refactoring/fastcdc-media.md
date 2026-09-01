# FastCDC Media

> 状态：FC-02 已定义 feature-gated v1 manifest wire contract；对象 scope、上传/完成生命周期和 HTTP API 由后续 FC-03～FC-07 补齐。默认构建不启用该能力。

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

## Responses and capabilities

prepare 响应使用 `manifest_id` 和 `missing_chunks`；已发布 manifest 响应使用 `manifest_id` 和 `manifest`。v1 capability payload 固定声明：

- `version: "1"`、`chunked_lfs: true`、`chunk_algorithms: ["fastcdc-v1"]`、`hash_algorithms: ["sha256"]`；
- `max_chunk_size: 8388608`、`max_manifest_size: 10485760`；
- `supports_batch_exists: true`、`supports_range_read: false`、`supports_standard_lfs_fallback: true`；
- `scope: "authenticated-user-and-repository"`。

此文档尚不声明公开 route 或上传生命周期；这些内容在对应实现可用后追加，避免向 feature-off 用户暴露未实现的协议面。
