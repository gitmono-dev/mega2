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
  - upload：`PUT …/manifests/{id}/chunks/{hash}`（`application/octet-stream`）
  - finalize：`POST …/manifests/{id}/finalize`
  - get：`GET …/manifests/by-media/{oid}` → `{manifest_id, manifest}`
  - chunk download：`GET …/manifests/by-media/{oid}/chunks/{hash}`

## Scope / key（FC-03）

- Namespace 字符串 `media` 只追加，不改 git/lfs/log/artifact/attachment/oci。
- Scope = 服务端 actor + canonical 绝对仓库路径；digest = SHA-256(`actor || 0x00 || repo`)。
  Actor 取 `AccessTokenUser` 的 `website_user_id`（非空）否则 `username`。请求体 actor/repository/`created_by` 不得覆盖。
- Object key：`v1/{scope_digest}/{pending|chunk|manifest|finalized}/{id}`，存储路径 `media/` + key（Media 不走 3-level sharding）。
- 对外错误不泄漏 digest、object key 或认证信息。

## 生命周期（FC-05 prepare / upload / resume）

- `prepare` 只接受已 `validate` 的 manifest，并**强制** `fallback_oid = media_oid`。
- pending 对象：`media/v1/{scope}/pending/{manifest_id}`，逻辑 TTL **24 小时**，JSON ≤ **10 MiB**。过期 session 不能继续 upload/get。
- `missing_chunks` 按 scoped chunk key 检查存在性；重复 `chunk_hash` 在响应中去重（保序）。
- chunk 上传必须命中 pending 声明的 hash/length，并校验实际 SHA-256（≤ 8 MiB）。同 scope 同 hash 的正确对象可幂等复用；已存错误内容返回 Conflict。
- finalize 前只能通过 pending manifest 声明的 hash 读取 chunk，不能按任意 hash 取对象。
- 领域错误：`Invalid` / `NotFound` / `Conflict` / `Storage` / `Io` / `Json`。存储错误对外固定为 `media object store error`。

## Finalize / fallback（FC-06）

- 同时最多 **2** 个 finalize（semaphore）。按 pending 声明的顺序逐块读取（≤8 MiB），写入临时文件并增量 SHA-256。
- 校验整对象 `media_oid`/`media_size` 后，再跑一遍 `fastcdc-v1`：offset/length/hash 必须与 manifest 完全一致。
- 缺失或损坏 chunk：**不**写 LFS namespace、**不**写 `lfs_objects`、**不**发布 finalized manifest；临时文件在成功和失败路径都删除。
- 通过后：`put_stream_bounded` 发布到 `lfs/{oid}`，`lfs_objects` 以 `ON CONFLICT (oid) DO NOTHING` 幂等插入，重新读取 metadata 并确认对象存在，然后才写 `media/v1/{scope}/finalized/{media_oid}`。
- 已存在的 finalized 若 `manifest_id`/`media_oid` 不一致则 Conflict；重复 finalize 在内容一致时成功。

## 出站事件（plan-20260912 / WH-06，已交付）

- 既有 finalize 路径在**本次实际写入 finalized manifest 成功**后发一次 `lfs.media.finalized`（契约见 [`storage-events.md`](storage-events.md)）：scope 只填服务端 `MediaScope` 的 canonical `repo_path`（HTTP 上通常带 `.git` 后缀），data 为 `oid,size,manifest_id,transfer="fastcdc"`；`event_id` 为 UUID v4。
- prepare / chunk / fallback 中间步骤不发；已存在 finalized 的 no-op 不发；finalize 中途失败不发。existing-check 与 put 非原子，并发 finalize（同进程或跨进程）可重复通知（本计划不新增唯一性锁/表）。
- 认证不改：Media 路由仍全部要求 `AccessTokenUser` 的 DB access token；匿名与静态 push token 请求仍 401 且零事件。storage-only 的 `push_auth=none` 不影响独立 ingest token 的认证与租户归属。
- 测试：`finalize::tests::storage_event_finalize_matrix`（真实 service finalize / no-op / 部分失败 / repo 过滤 / emitter 故障）、`lfs_media::tests::storage_event_auth_reachability`（匿名 / 静态 token / DB token 三类请求）、进程级 `integration_storage_events_media`（feature-on/off 两个独立 target-dir 二进制的路由与认证矩阵）。

## HTTP / auth / OpenAPI（FC-07）

- Feature-on 时挂在当前 LFS mount 下：逻辑前缀 `libra/media/v1`。仓库 URL `<repo>.git` 的外部路径是 `<repo>.git/info/lfs/libra/media/v1/...`；OpenAPI 登记为 `/api/v1/lfs/libra/media/v1/...`。Feature-off 不注册这些路由（运行时 404，schema 中也不出现）。
- **每一条** Media 路由（含 capabilities）都要求 `AccessTokenUser`（Bearer）。不复用标准 LFS objects 的「batch 后裸 URL 可不再认证」例外。
- 仓库路径取 URI 改写前保存在 `LfsRepoContext` 的原始前缀（例如 `/acme/app.git`）。缺少合法前缀 → 400。
- Body 上限在 handler 前拒绝：JSON/manifest 路由 `DefaultBodyLimit` 10 MiB；chunk PUT 路由 8 MiB。另有 `Content-Length` 中间件在读 body 前返回 413。
- 错误：Invalid/Json → 400，NotFound → 404，Conflict → 409，Storage/Io → 500 且 body 固定 `media object store error`。
- Capabilities JSON 来自 `Capabilities::v1()`：`fastcdc-v1`、sha256、上限、`supports_batch_exists`、`supports_standard_lfs_fallback`。
- 普通 LFS `/objects`、`/locks`、`/objects/batch` 行为不变。

固定 fixture：`src/ceres/lfs/media/fixtures/`（合法 `valid_v1.json` / 空文件 `empty.json` / 非法 version 与 fallback）。`valid_v1.json` 字节 SHA-256：`20226243095e92274b3683f4c09bcd12ae35d245b073b38296a2a895c13b8c9d`。

## 双仓 interop gate（FC-15）

默认 `cargo test --all` **不**要求 sibling Libra checkout。真实 client/server 证据只在显式 ignored target 下执行：

```bash
export LIBRA_DIR=/path/to/libra          # 干净 checkout，HEAD == LIBRA_INTEROP_REV
export LIBRA_INTEROP_REV=d1aafb23dccb77408ac43786b173f1c9a0d760aa
source .env.test
cargo test -p monoengine --features fastcdc --test integration_fastcdc_libra \
  -- --ignored --exact monoengine_libra_fastcdc_interop --test-threads=1
```

Harness 启动 `--features fastcdc` 的 `service http`，写入仅当前测试可读的 ready-file（JSON：`lfs_url` 必须为 `<repo>.git/info/lfs/` 且带尾 `/`，加一次性 `token`），再精确运行 Libra `monoengine_fastcdc_http_interop`。缺失 `LIBRA_DIR`、脏树、错误 revision 或没有 `cargo` 时失败，不得 SKIP-green。token 与完整 URL 不会写入失败输出或计划证据。

Feature-off 对照（独立 target dir 构建未启用 feature 的 binary）：

```bash
CARGO_TARGET_DIR="$PWD/target/fastcdc-off" cargo build -p monoengine
export MONOENGINE_FASTCDC_OFF_BIN="$PWD/target/fastcdc-off/debug/monoengine"
source .env.test
cargo test -p monoengine --features fastcdc --test integration_fastcdc_libra \
  -- --ignored --exact monoengine_fastcdc_feature_off_falls_back --test-threads=1
```

feature-off 时 `<repo>.git/info/lfs/libra/media/v1/capabilities` 为 404，Libra `media probe` 选择标准 LFS fallback。这不是标准 Git FastCDC 互通。

