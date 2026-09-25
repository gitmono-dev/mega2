# FastCDC Media（git-internal / Libra extension）

本文记录 `fastcdc` feature 下 Media 协议的服务端契约。这不是标准 Git LFS 扩展，也不是标准 Git BLAKE3 互通。`media_oid`、chunk hash 与 fallback OID 属于 **LFS SHA-256** digest domain，不受仓库 `monorepo.object_format` / Git `HashKind` 影响。

## 协议（FC-02 / MF-02）

- 算法：`fastcdc-v2020-32k`（`fastcdc = "=3.2.1"`，v2020，Normalization::Level1，seed=0，min/avg/max = 32/64/256 KiB）。旧 `fastcdc-v1` / 对象前缀 `v1/` 命名空间保留不读写、不自动删除。
- Manifest：`version=1`，`hash_algorithm=sha256`，chunk `compression` 固定 `none` 且 `encoded_length == length`。
- 块限制：非尾块 `32768..=262144`；尾块 `1..=262144`；空文件零块。不设总 chunk 数或全文件字节产品上限。
- Canonical ID：SHA-256（小写 hex）over JSON of `(version, algorithm, hash_algorithm, media_oid, media_size, chunks)`。`created_by` 与 `fallback_oid` 不进入 identity；分页边界不参与身份（P-01）。
- 包装上限：Media metadata 单页/摘要/状态请求响应含包装 ≤ **1 MiB**（`MAX_ENVELOPE_SIZE`）；单页最多 **4096** 条且紧凑 entries JSON ≤ **960 KiB**（P-01a）；chunk payload ≤ 256 KiB；`created_by` ≤ 4096 bytes。完整 LFS 流不受页限额约束。
- `fallback_oid` 若存在必须等于 `media_oid`。
- HTTP 形状（路径以 Libra client 为准）：
  - capabilities：`GET <repo>.git/info/lfs/libra/media/v1/capabilities`
  - prepare：`POST …/manifests` → `{manifest_id, missing_chunks}`（唯一 hash 差集；exists 并发 ≤16）
  - page：`PUT …/manifests/{id}/pages/{page_no}`（幂等；内容冲突 409）
  - seal：`POST …/manifests/{id}/seal` → `{manifest_id, seal_generation, page_count}`
  - missing：`GET …/manifests/{id}/missing?cursor=` → `{hashes, next_cursor}`
  - upload：`PUT …/manifests/{id}/chunks/{hash}`（`application/octet-stream`）
  - finalize：`POST …/manifests/{id}/finalize` → 202 任务（MF-03）；`GET …/tasks/{task_id}` 轮询
  - get：`GET …/manifests/by-media/{oid}` → `{manifest_id, manifest}`
  - chunk download：`GET …/manifests/by-media/{oid}/chunks/{hash}`

## Capabilities（共享表）

`Capabilities::v1()` 同时保留旧字段名并输出共享表字段：

| 字段 | 值 |
| --- | --- |
| version | `"1"` |
| batch_exists / supports_batch_exists | true |
| range_read / supports_range_read | false |
| standard_lfs_fallback / supports_standard_lfs_fallback | true |
| supports_manifest_id_read | true |
| manifest_paging | `"v1"` |
| max_page_entries | 4096 |
| max_page_bytes / max_manifest_size | 1048576 |
| chunk_algorithms | `["fastcdc-v2020-32k"]` |
| max_chunk_size | 262144 |

## Scope / key（FC-03 / C-08）

- Namespace 字符串 `media` 只追加，不改 git/lfs/log/artifact/attachment/oci。
- Scope = 服务端 actor + canonical 绝对仓库路径；digest = SHA-256(`actor || 0x00 || repo`)。
  Actor 取 `AccessTokenUser` 的 `website_user_id`（非空）否则 `username`。请求体 actor/repository/`created_by` 不得覆盖。
- Object key：`fastcdc-v2020-32k/{scope_digest}/{pending|chunk|manifest|finalized|page}/{id}`，存储路径 `media/` + key（Media 不走 3-level sharding）。旧 `v1/` 对象隔离保留。
- 对外错误不泄漏 digest、object key 或认证信息。

## 持久分页状态（FC-16 / MF-08）

服务库三表（迁移 `m20260925_000100_media_paging`）：

- `media_session`：`(scope_digest, manifest_id)`；`pending|sealed|finalized`；`seal_generation` 绑定缺块 cursor。
- `media_entry`：派生 chunk 索引；跨页 hash→length 冲突拒绝。
- `media_task`：异步 finalize lease（MF-03/07）。

`prepare` 在对象存储写入 pending 的同时 `upsert_pending_session`；`put_page` / `seal` / `missing` 走 `MediaPagingStorage` + page blob。

## Prepare / chunk（FC-05）

- `prepare` 只接受已 `validate` 的 manifest（≤1 MiB 包装），并**强制** `fallback_oid = media_oid`；按 P-01a 计算 `page_count`。
- pending 对象：`media/fastcdc-v2020-32k/{scope}/pending/{manifest_id}`，逻辑 TTL **24 小时**。过期 session 不能继续 upload/get。
- `missing_chunks` 按 scoped chunk key 检查存在性（并发 ≤16）；重复 `chunk_hash` 在响应中去重（保序）。
- chunk 上传必须命中 pending 声明的 hash/length，并校验实际 SHA-256（≤ 256 KiB）。同 scope 同 hash 的正确对象可幂等复用；已存错误内容返回 Conflict。
- 领域错误：`Invalid` / `NotFound` / `Conflict` / `Storage` / `Io` / `Json`。存储错误对外固定为 `media object store error`。

## Finalize / fallback（FC-06 / MF-03）

- 同时最多 **2** 个 finalize（semaphore）；活跃排队上限 **128**（超限 429 + `Retry-After`）。
- `POST …/manifests/{id}/finalize` **仅接受 sealed 会话**，返回 **202** `{task_id,manifest_id,state,status_url}`；`status_url` 为同前缀 `libra/media/v1/tasks/{task_id}`（客户端不得跨 origin 跟随或转发 token）。
- `GET …/tasks/{task_id}` 每请求重新做 scope 认证，返回 `state/bytes_verified/pages_verified/retryable/error_code`；`complete` 另含 `oid/size`。
- 验证：按 sealed 页序遍历 `media_entry`，逐块 scoped 读并校验 length/hash；连续覆盖 `media_size`；整对象 SHA-256 须等于 `media_oid`。**不**要求新鲜 FastCDC 冷切边界相等。
- 磁盘写与哈希在 `spawn_blocking`；单次 I/O ≤120s、无进展 ≤10min、lease 60s（约每 20s 续租）；持续进展不受整文件墙钟限制。
- 缺失或损坏 chunk / gap / overlap / overflow：**不**写 LFS namespace、**不**写 `lfs_objects`、**不**发布 finalized；临时文件与 lease 在成功/失败/取消路径均释放。
- 通过后：发布到 `lfs/{oid}`，`lfs_objects` 幂等插入，再写 `media/fastcdc-v2020-32k/{scope}/finalized/{media_oid}`（多布局原子发布见 MF-07）。
- 已存在的 finalized 若 `manifest_id`/`media_oid` 不一致则 Conflict；重复 finalize 在内容一致时成功。

## 出站事件（plan-20260912 / WH-06，已交付）

- 既有 finalize 路径在**本次实际写入 finalized manifest 成功**后发一次 `lfs.media.finalized`（契约见 [`storage-events.md`](storage-events.md)）：scope 只填服务端 `MediaScope` 的 canonical `repo_path`，data 为 `oid,size,manifest_id,transfer="fastcdc"`。
- prepare / chunk / fallback 中间步骤不发；已存在 finalized 的 no-op 不发。
- 认证不改：Media 路由仍全部要求 `AccessTokenUser`。

## HTTP / auth / OpenAPI（FC-07）

- Feature-on 时挂在当前 LFS mount 下：逻辑前缀 `libra/media/v1`。OpenAPI 登记为 `/api/v1/lfs/libra/media/v1/...`。
- **每一条** Media 路由（含 capabilities）都要求 `AccessTokenUser`（Bearer）。
- Body 上限：JSON/metadata 路由 `DefaultBodyLimit` **1 MiB**；chunk PUT 路由 **256 KiB**。另有 `Content-Length` 中间件在读 body 前返回 413。
- 错误：Invalid/Json → 400，NotFound → 404，Conflict → 409，Storage/Io → 500 且 body 固定 `media object store error`。

固定 fixture：`src/ceres/lfs/media/fixtures/`（合法 `valid_v1.json` / 空文件 `empty.json` / 非法 version 与 fallback）。`valid_v1.json` 字节 SHA-256：`09ae74a7f69da0bbd2b3df12d8f4bb85413b8ba34ec5de123f68141219ac3b96`。

## 双仓 interop gate（FC-15）

默认 `cargo test --all` **不**要求 sibling Libra checkout。真实 client/server 证据只在显式 ignored target 下执行（见既有 `integration_fastcdc_libra` 说明）。feature-off 时 Media 能力探测为 404，Libra 选择标准 LFS fallback。
