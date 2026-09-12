# FastCDC Media（git-internal / Libra extension）

本文记录 `fastcdc` feature 下 Media 协议的服务端契约。这不是标准 Git LFS 扩展，也不是标准 Git BLAKE3 互通。`media_oid`、chunk hash 与 fallback OID 属于 **LFS SHA-256** digest domain，不受仓库 `monorepo.object_format` / Git `HashKind` 影响。

## 协议（FC-02）

- 算法：`fastcdc-v1`（min 512 KiB / avg 2 MiB / max 8 MiB）。
- Manifest：`version=1`，`hash_algorithm=sha256`，chunk `compression` 固定 `none` 且 `encoded_length == length`。
- Canonical ID：SHA-256（小写 hex）over JSON of `(version, algorithm, hash_algorithm, media_oid, media_size, chunks)`。`created_by` 与 `fallback_oid` 不进入 identity。
- 上限：最多 8192 chunks；manifest JSON ≤ 10 MiB；单 chunk ≤ 8 MiB。
- `fallback_oid` 若存在必须等于 `media_oid`。
- HTTP 形状（路由由 FC-07 注册，路径以 Libra client 为准）：
  - capabilities：`GET <repo>.git/info/lfs/libra/media/v1/capabilities`
  - prepare：`POST …/manifests` → `{manifest_id, missing_chunks}`
  - get：`GET …/manifests/by-media/{oid}` → `{manifest_id, manifest}`

## Scope / key（FC-03）

- Namespace 字符串 `media` 只追加，不改 git/lfs/log/artifact/attachment/oci。
- Scope = 服务端 actor（`website_user_id`）+ canonical 绝对仓库路径；digest = SHA-256(`actor || 0x00 || repo`)。
- Object key：`v1/{scope_digest}/{pending|chunk|manifest|finalized}/{id}`，存储路径 `media/` + key（Media 不走 3-level sharding）。
- 请求体 actor/repository/`created_by` 不得覆盖 scope。对外错误不泄漏 digest、object key 或认证信息。

## 生命周期（FC-05 prepare / upload / resume）

- `prepare` 只接受已 `validate` 的 manifest，并**强制** `fallback_oid = media_oid`。
- pending 对象：`media/v1/{scope}/pending/{manifest_id}`，逻辑 TTL **24 小时**，JSON ≤ **10 MiB**。过期 session 不能继续 upload/get。
- `missing_chunks` 按 scoped chunk key 检查存在性；重复 `chunk_hash` 在响应中去重（保序）。
- chunk 上传必须命中 pending 声明的 hash/length，并校验实际 SHA-256（≤ 8 MiB）。同 scope 同 hash 的正确对象可幂等复用；已存错误内容返回 Conflict。
- finalize 前只能通过 pending manifest 声明的 hash 读取 chunk，不能按任意 hash 取对象。
- 领域错误：`Invalid` / `NotFound` / `Conflict` / `Storage` / `Io` / `Json`。存储错误对外固定为 `media object store error`。

固定 fixture：`src/ceres/lfs/media/fixtures/`（合法 `valid_v1.json` / 空文件 `empty.json` / 非法 version 与 fallback）。`valid_v1.json` 字节 SHA-256：`20226243095e92274b3683f4c09bcd12ae35d245b073b38296a2a895c13b8c9d`。
