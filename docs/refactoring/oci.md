# OCI Distribution（storage-only 容器镜像仓库）

本文是 monoengine **storage-only** 形态下 OCI Distribution `/v2` 面的架构与数据流事实源。产品边界与任务追溯见 [`../plan/plan-20260902.md`](../plan/plan-20260902.md)。对象命名空间契约见 [`orbit.md`](./orbit.md)。进程级 IT 见 [`integration.md`](./integration.md) 的 `integration_oci` 行。部署启用步骤见 [`../deploy-trunk.md`](../deploy-trunk.md) 第 10 节。

> **挂载门（ADR-DR-01）：** `/v2` 仅在 `git.storage_only()`（显式 `git.push_auth`）且 `[oci].enabled=true` 时注册。review 形态或不显式启用时整面不存在（裸 404）。`enabled=true` 且非 storage-only → 启动拒绝。

## 端点表

路径中 `{name}` 可为多段（如 `team/image`）。路由实现为单条 `/{*tail}` catch-all + 派发器（见下节），不是 axum 分段 `{name}`。

| 方法 | 路径 | 成功 | 备注 |
|---|---|---|---|
| `GET` | `/v2/` | `200` | Registry ping；认证跟随读策略（ADR-DR-02） |
| `GET`/`HEAD` | `/v2/{name}/manifests/{reference}` | `200` / `304` | `reference` = tag 或 `sha256:<hex>`；ETag / `If-None-Match` → 304 |
| `PUT` | `/v2/{name}/manifests/{reference}` | `201` | 4MiB 上限；mediaType 白名单；引用校验 |
| `DELETE` | `/v2/{name}/manifests/{reference}` | — | 恒 `UNSUPPORTED`（405 封套） |
| `GET`/`HEAD` | `/v2/{name}/blobs/{digest}` | `200` / `206` | 单区间 `Range` → 206；多区间/`If-Range` 忽略按全量 200 |
| `DELETE` | `/v2/{name}/blobs/{digest}` | — | 恒 `UNSUPPORTED`（405 封套） |
| `POST` | `/v2/{name}/blobs/uploads/` | `202` / `201` | 建会话；或 `?digest=` 单体；或 `?mount=&from=` 跨仓挂载 |
| `PATCH` | `/v2/{name}/blobs/uploads/{uuid}` | `202` | 分片追加；可选 `Content-Range` |
| `GET` | `/v2/{name}/blobs/uploads/{uuid}` | `204` | 会话状态（`Range` + UUID） |
| `PUT` | `/v2/{name}/blobs/uploads/{uuid}?digest=` | `201` | complete（两遍流式） |
| `DELETE` | `/v2/{name}/blobs/uploads/{uuid}` | `204` | 取消会话并清分片 |
| `GET` | `/v2/{name}/tags/list` | `200` | `n`（≤100）/`last` 分页 + `Link` |

未注册：`_catalog`、referrers、独立 token 颁发（`/v2/auth`）。

## 路由派发器（ADR-DR-08）

`/v2` 下注册单条 `/{*tail}`（空 tail = ping）。handler 按路径**后缀**分派，剩余前缀为 `name`：

1. `blobs/uploads/<uuid>` → 上传会话族
2. `blobs/uploads/` → 建会话 / mount / 单体
3. `blobs/<digest>` → blob 读/删
4. `manifests/<reference>` → manifest 读/写/删
5. `tags/list` → 标签列表
6. 空 → ping
7. 其它 → `NAME_INVALID`（或会话相关码）

`name` 校验对齐 distribution `reference.NameRegexp` 等价规则；段内禁止 `..`。实现：`src/api/router/oci_router.rs`。

OpenAPI 无法为 catch-all 生成细粒度路径；运行时聚合说明由 DR-11 的 `include_oci` 控制（storage-only + enabled 才合并 OCI paths）。

## 键布局（ADR-DR-07）

全部键在对象存储命名空间 `oci`（`ObjectNamespace::Oci`，`as_str()="oci"`）下：

| 键模式 | 用途 |
|---|---|
| `blobs/sha256/<hex>` | CAS blob（层/config） |
| `manifests/sha256/<hex>` | 原始 manifest JSON 字节 |
| `uploads/<uuid>/<seq>` | 上传分片；`seq` 从 0 递增十进制 |

`ObjectKey` 校验与 `default_sharding()` 沿用既有逻辑。元数据在 Postgres（无 link 文件）：

| 表 | 主键 / 要点 |
|---|---|
| `oci_manifest` | `(repo_name, digest)` + `media_type` / `size` |
| `oci_tag` | `(repo_name, tag)` → `digest` |
| `oci_blob_ref` | `(repo_name, digest)` 成员关系（manifest 引用校验 / 未来 GC） |
| `oci_upload` | `uuid` → `repo_name` / `offset` / `chunks` |

无 `oci_repository` 表：repo 存在性 = 有 manifest/tag/blob_ref 行。

## 认证矩阵（ADR-DR-02）

不实现独立 OCI token 服务。凭据复用 git push token：

| 输入 | 解析 |
|---|---|
| `Authorization: Bearer <token>` | 直取 token |
| `Authorization: Basic …` | 取 **password** 为 token（username 任意，仅日志） |
| 401 挑战 | `WWW-Authenticate: Basic realm="monoengine registry", Bearer realm="monoengine registry"` |

| 操作 | `anonymous_access=true` | `anonymous_access=false` | `push_auth=none` | `push_auth=token` |
|---|---|---|---|---|
| 读（GET/HEAD manifest/blob、tags/list、`GET /v2/` ping） | 无凭据放行；无效凭据仍 401 | 需有效 token | 读策略同上 | 读策略同上；token 覆盖检查仅写侧 |
| 写（POST/PATCH/PUT/DELETE） | — | — | 全放行 | `lookup_push_token` + `token_covers_repo("/" + repo)`；覆盖失败 → `DENIED` |

`docker login <host> -u <任意> -p <push token>` 即完成认证。跨仓 mount 要求**目标写授权 + 源读授权**（GC-DR-02）。

## 错误码清单（DR-05，16 码）

响应封套：`{"errors":[{"code","message","detail"?}]}`。`detail` 空则省略。实现：`src/ceres/oci/error.rs`。

| 码 | HTTP | 典型触发 |
|---|---|---|
| `NAME_UNKNOWN` | 404 | 未知仓库 |
| `NAME_INVALID` | 400 | 仓库名非法 / 未知路径形状 |
| `MANIFEST_UNKNOWN` | 404 | 未知 tag/digest |
| `MANIFEST_INVALID` | 400 | mediaType/体过大/JSON 非法 |
| `MANIFEST_BLOB_UNKNOWN` | 400 | manifest 引用的 blob/子 manifest 缺失 |
| `BLOB_UNKNOWN` | 404 | 仓库无该 blob 成员 |
| `BLOB_UPLOAD_UNKNOWN` | 404 | 未知上传会话 |
| `BLOB_UPLOAD_INVALID` | 404 | 上传会话非法 |
| `DIGEST_INVALID` | 400 | digest 非 `sha256:<hex64>` 或与内容不符 |
| `SIZE_INVALID` | 400 | `Content-Length` 与区间不符 |
| `RANGE_INVALID` | 416 | `Content-Range` 与会话 offset 不符 / 并发冲突 |
| `TAG_INVALID` | 400 | tag 非法 |
| `UNAUTHORIZED` | 401 | 缺/无效凭据（附挑战头） |
| `DENIED` | 403 | token 有效但不覆盖 repo |
| `UNSUPPORTED` | 405 | manifest/blob DELETE |
| `PAGINATION_NUMBER_INVALID` | 400 | `tags/list` 的 `n` 非法 |

## 上传状态机（ADR-DR-06）

```text
POST /blobs/uploads/          → 建 oci_upload(offset=0,chunks=0) → 202 + Location/UUID
     │
     ├─ POST ?digest=         → 单体：收流写分片 → finalize（同下）→ 201
     ├─ POST ?mount=&from=    → 源有 blob_ref → 目标写 ref → 201；否则退化为新会话
     │
PATCH /uploads/{uuid}         → 写 uploads/<uuid>/<seq>；条件更新 offset/chunks
GET   /uploads/{uuid}         → 204 + Range: 0-<offset-1>
PUT   /uploads/{uuid}?digest= → finalize_blob（两遍流式）→ 201
DELETE /uploads/{uuid}        → 按 seq 删分片 + 删行 → 204
```

**两遍流式 complete（常数内存）：**

1. **Pass A：** 按 seq `0..chunks-1` 流式读分片，仅算 sha256；与请求 digest 不符 → `DIGEST_INVALID`（会话/分片保留）。
2. **Pass B：** 再流式读分片 → `put_stream(blobs/sha256/<actual>)`（键由已验证 digest 决定）。
3. 成功：写 `oci_blob_ref`、删分片、删会话行。

总成本固定 2 读 + 1 写 O(size)。**不使用** `exists → skip` 快捷路径。

## 发布出站事件（plan-20260912 / WH-04，已交付）

一次 manifest 发布（`PUT /v2/{name}/manifests/{reference}`）在对象写入 + `oci_manifest` 行 + 可选 tag upsert **全部成功**后发出一次 `oci.manifest.published` 出站事件（契约见 [`storage-events.md`](storage-events.md)）：

- scope 只填 `oci_repository`（规范 repo 名），data 为 `digest,reference,media_type,size`；`event_id` 为 UUID v4。
- 分步写入不是跨对象/DB 原子事务：任何一步失败即整体报错、不发事件；重复 PUT 可重复通知；后续 tag 改写不影响已发快照。
- chunk / blob / mount 不产生事件（它们不是发布）；digest 不符、非法 tag、超限 body 等拒绝路径不发。
- 过滤按 `oci_repositories` 精确匹配；事件构造或投递失败不改变已提交的发布结果。

进程级证据：`integration_oci` 的 `integration_oci_storage_events_publication`；router lib collector 覆盖过滤与晚到快照（`storage_event_publication_matrix` / `storage_event_delayed_snapshot`）。

**崩溃窗口：**

| 窗口 | 残留 | 重试语义 |
|---|---|---|
| PATCH 分片已写、DB 未更新 | 孤儿分片（不可达） | 无害；GC 延后（DEFER-DR-02/03） |
| Pass A 后崩溃 | 会话原样 | 重试无副作用 |
| Pass B 中途崩溃 | CAS 键可能有部分字节，无 `oci_blob_ref` | 读者不可见；重试 complete 覆写同键（内容寻址） |

## 参照 pin 与差异

| 参照 | Pin | 用途 |
|---|---|---|
| docker/distribution | `5b354e6fda6126a188df7d3769bfdfb02d8d020b` | 协议语义事实源（errcode 状态码、uploads 状态机、tags 分页、Range 行为） |
| rk8s `project/distribution/` | `c862eea340b63aef22435614793cc668aa77b926` | Rust 结构参照：`/{*tail}` + `dispatch_handler` |
| spegel | `a8d80fdd01639a2f247098a4cc6964ff0d030fc7` | 只读对照（P2P/代理非目标） |

本地 checkout（计划成稿时）：`/run/media/genedna/data/tmp/{distribution,rk8s,spegel}`。缺失时以 OCI Distribution Spec v1.1 文本为准，不得凭空造端点。

**相对 distribution 的主要差异：**

| 主题 | distribution | monoengine |
|---|---|---|
| 元数据 | 存储驱动 link 文件 | Postgres 4 表 + 对象存储字节 |
| 认证 | 可配 token 服务 / htpasswd 等 | 静态 Basic/Bearer，复用 `[[git.push_tokens]]` |
| 上传 complete | HMAC `_state` 等 | DB 会话 + 两遍流式；CAS 键仅验证后写入 |
| 删除 | 可启用 delete | 路由存在，恒 `UNSUPPORTED` |
| `_catalog` / referrers | 可选 | 不实现 |
| digest | 以 sha256 为主 | **仅** `sha256:<hex64>` |
| 挂载条件 | 独立 registry 进程 | 仅 storage-only + `[oci].enabled` |

**相对 rk8s：** 派发器形状对齐；存储与认证落到 monoengine 既有 Postgres / orbit / push token，不引入 rk8s 的独立 registry 运行时。

## 边界与非目标

本切片**实现**：最小 push/pull + `tags/list`；多段 repo 名；chunked / monolithic / mount 上传；Basic 静态凭据流；匿名读跟随 `git.anonymous_access`。

**非目标（见计划 DEFER-DR-*）：**

- `_catalog` / repo 枚举（DEFER-DR-01）
- manifest/blob 删除语义与跨仓 GC（含上传孤儿清扫）（DEFER-DR-02）
- S3 multipart / copy 加速 complete（DEFER-DR-03）
- referrers API（DEFER-DR-04）
- manifest list 平台重写（老 docker 客户端）（DEFER-DR-05）
- compose smoke profile 集成（DEFER-DR-06）
- 独立 OCI token 颁发、pull-through / P2P、review 形态挂载 `/v2`、sha256 以外的 digest 算法

## 相关入口

| 路径 | 角色 |
|---|---|
| `src/api/router/oci_router.rs` | HTTP 派发与端点 |
| `src/ceres/oci/` | 错误码、认证、digest、manifest 模型 |
| `src/jupiter/service/oci_service.rs` | 对象层（CAS / 分片 / finalize） |
| `src/jupiter/storage/oci_db_storage.rs` | 元数据 CRUD |
| `config/config-storage-only.toml` `[oci]` | 样例启用 |
| `scripts/oci_smoke_storage_only.sh` | 宿主机 docker CLI live smoke |
| `tests/integration_oci.rs` | 进程级黑盒 IT |
