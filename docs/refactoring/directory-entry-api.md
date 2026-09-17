# 目录变更与标签 HTTP 契约（storage-only / trunk）

本页是 **Mega2** storage-only（`push_policy=trunk`）形态下**目录变更**与 **monorepo 标签** 产品 HTTP 的单一契约正文，供外部调用方按 `file:line` pin。计划出处 [`../plan/plan-20260917.md`](../plan/plan-20260917.md)（ADR-LB-01..07）。

主消费者是 sibling Libra 的 `libra mega2 browser` TUI。写本页的目的，是让调用方读契约而不是猜路径。

## 状态标记

每条路由标注实现状态。本页由 LB-01 创建（当时不含任何代码改动）；此后每张实现卡在落地时把自己的路由翻为 `implemented`，其余路由仍是 `specified-unimplemented`——那些小节描述的是落地后的契约，不是今日行为。

| 状态 | 含义 |
|---|---|
| `implemented` | 程式已存在且已挂到 storage-only，可直接调用 |
| `specified-unimplemented` | 本页冻结 wire；storage-only 上**今日不可用**（404 或行为不同） |

| 路由 | 状态 | 承接卡 | 今日实况 |
|---|---|---|---|
| `GET /api/v1/tree` | `implemented` | — | 可用 |
| `POST /api/v1/create-entry` | `implemented` | — | 可用 |
| `POST /api/v1/delete-entry` | `implemented` | LB-02 | 可用：`write_routers` 已登记（Review 与 trunk 都有） |
| `POST /api/v1/move-entry` | `implemented` | LB-03 | 可用：`write_routers` 已登记（Review 与 trunk 都有） |
| `POST /api/v1/tags` | `specified-unimplemented` | LB-04 | handler 已存在但只挂在 Review；storage-only **404**。且**无任何鉴权**，见「鉴权」 |
| `POST /api/v1/tags/list` | `specified-unimplemented` | LB-04 | 同上（读，无鉴权问题） |
| `GET /api/v1/tags/{name}` | `specified-unimplemented` | LB-04 | 同上（读） |
| `DELETE /api/v1/tags/{name}` | `specified-unimplemented` | LB-04 | 同上。且**无任何鉴权**，见「鉴权」 |

公共前缀 `/api/v1` 由外层 nest 施加。

### 成功包络

一律是 `CommonResult<T>`（`src/contract/api/common.rs:4-9`）：

```json
{ "req_result": true, "data": { }, "err_message": "" }
```

三个键**始终存在**：`req_result: bool`、`data: Option<T>`（失败时为 `null`）、`err_message: String`（成功时为空串）。

## 范围

- **只 pin 不改语义：** `GET /tree`、`POST /create-entry`。
- **新增产品写：** `POST /delete-entry`、`POST /move-entry`（改名 = 同 parent 的 move）。
- **挂载 + 鉴权 + 文档对齐：** 四条 `/tags*`。
- 目录变更是**父目录 tree 改写**后写新 commit，经 `land_api_tip_push`（trunk）或既有 CL 分支（Review）前进 tip；**不是** Git delete command，也不走 CL `apply_changes`（ADR-LB-02）。

## 已有 API（本计划不改语义）

### `GET /api/v1/tree` — `implemented`

| 项 | 值 |
|---|---|
| 鉴权 | 无（不送 Authorization） |
| Query | `path`（可选，`#[serde(default = "default_path")]` → `/`）、`refs`（可选，`#[serde(default)]` → 空串 = 当前 tip） |
| 成功 | `200` + `CommonResult<TreeResponse>` |

`data.tree_items[]` 每项为 `{ name, path, content_type }`（`TreeBriefItem`）。

> `data.file_tree` 是祖先辅助结构，**不得**当导航权威——导航只用 `tree_items`。

### `POST /api/v1/create-entry` — `implemented`

| 字段 | 规则 |
|---|---|
| `is_directory` | 必填 `bool`。目录取 `true` |
| `name` | 必填 |
| `path` | 必填。父目录，rooted；根下用 `/` |
| `content` | 目录创建时可省略或为 `null`；`is_directory=false` 时**必填**（缺失见「错误映射」） |
| `author_username` / `author_email` | 可选 |
| `skip_build` | **可选**，`#[serde(default)]` → `false`。Libra 一律送 `true` |
| `mode` | **可选**，默认 `EditCLMode::TryReuse(None)`。调用方应**省略**该键，**不得**送字符串 `"try_reuse"` |

成功 `data` 为 `CreateEntryResult { commit_id, new_oid, path, cl_link }`；trunk 上 `cl_link` 必为 `null`；`path` 只作回执。

`is_directory=true` 时服务端写一个带时间戳的 `.gitkeep` 占位。

## 新增：目录变更

与 create-entry 对齐，用 **parent `path` + `name`**，不用单一绝对路径字段当权威。HTTP 状态同样是 **200** + `CommonResult`（**不用 204**）。

以下两表逐行复制自 ADR-LB-03，是 wire 的权威来源。

### `POST /api/v1/delete-entry` — `implemented`（LB-02）

```json
{
  "path": "/project",
  "name": "old-dir",
  "author_username": null,
  "skip_build": true
}
```

| 字段 | 规则 |
|---|---|
| `path` | **必填**。父目录，rooted，默认语义与 create-entry 相同（根下用 `/`） |
| `name` | **必填**。要删的目录名；禁止 `/`、`.`、`..`、分隔符、NUL、控制字符 |
| `author_username` | **可选**。`Option<String>`；可省略或 JSON `null`。不参与鉴权。 |
| `skip_build` | **可选**。`bool`，`#[serde(default)]` → `false`。Libra 一律送 `true`。与 create-entry 同形。 |
| 目标 | 必须已存在且 `content_type=directory`；禁止删 `/` |
| 鉴权 | `authorize_trunk_api_write(path)`，path = **父目录** |
| HTTP | **200** + `CommonResult`（不用 204） |
| 成功 `data` | `{ "commit_id": "<hex>", "path": "/project/old-dir", "cl_link": null }`；**无** `new_oid`。`path` 只作回执。 |

空目录与非空目录是**同一条**删除语义（删掉父 tree 中的该 item）；不提供「只删空目录」。

落地事实（LB-02，`mono_api_service.rs` 的 `delete_monorepo_entry`）：

- `path`/`name` 先过 `validate_entry_target`（`src/ceres/model/git.rs`）：`path` 须 rooted（空串等于 `/`，容忍一个尾随 `/`），组件不得为空、`.`、`..`；`name` 为单一组件，禁 `/`、`\`、`.`、`..`、NUL 与控制字符。不合规一律 **400**。
- 父目录被删空时，服务端补写一个带时间戳的 `.gitkeep`，父目录保留为**空目录**——与 create-entry 表示新建空目录的方式一致；Git 无法在路径上表示空 tree，这是唯一能保住父目录的做法。
- 同名的 blob 与 tree 可以并存（create-entry 的重名检查按 mode 区分），删除只匹配目录项；只有同名文件时报 400「不是目录」，都没有时报 404。
- 一次删除 = 一次 commit（父链 tree 改写 + `.gitkeep` 可选 blob），trunk 经 `land_api_tip_push` 前进 tip，Review 走既有 CL 分支（`EditCLMode::TryReuse(None)`，与 create-entry 相同的政策分流）。
- trunk 上父目录为 `/`（即删除顶层目录）时，B0 拒绝根 tip 经 MonoWriteQueue 前进，返回 **400**（`no non-root path tip under / for trunk API write`）；Review 形态则在 `/` 的 CL 上进行。
- `commit_id` 在 trunk 上是落地后的 tip；`path` 只作回执。

### `POST /api/v1/move-entry` — `implemented`（LB-03）

```json
{
  "from_path": "/project",
  "from_name": "old-dir",
  "to_path": "/project/other",
  "to_name": "new-dir",
  "author_username": null,
  "skip_build": true
}
```

| 字段 | 规则 |
|---|---|
| `from_path` / `from_name` | **必填**。源父 + 源名；源必须是 directory |
| `to_path` / `to_name` | **必填**。目标父 + 目标名；目标父必须已存在且为 directory |
| `author_username` | **可选**。同 delete-entry。 |
| `skip_build` | **可选**。同 delete-entry；Libra 一律送 `true`。 |
| 改名 | `from_path == to_path` 且 `from_name != to_name` |
| 拒绝 | 源目标相同；目标名已存在；把目录移进自己的子树；file 源；`/`；traversal；`import_dir` |
| 鉴权 | **两个** path（`from_path` 与 `to_path`）都必须通过 `authorize_trunk_api_write`；任一失败则不写 |
| HTTP | **200** + `CommonResult`（不用 204） |
| 成功 `data` | `{ "commit_id": "<hex>", "from_path": "/project/old-dir", "to_path": "/project/other/new-dir", "cl_link": null }`；**无** `new_oid`。返回路径只作回执。 |

落地事实（LB-03，`mono_api_service.rs` 的 `move_monorepo_entry`）：

- 两组 `path`/`name` 都先过 `validate_entry_target`（规则同 delete-entry）；父路径经 `normalize_parent_path` 归一（空串 = `/`，容忍一个尾随 `/`），改名 = 归一后 `from_path == to_path` 且名字不同。
- 校验顺序：源目标相同 → 移进自己的子树（目标父 = 源目录或其后代）→ 目标父落在 ImportRepo 下（router 只按 `from_path` 分派，monorepo handler 自查 `git_repo` 后以 **409** 拒绝）→ 源父不存在 / 源不存在 / 源是文件 → 目标父不存在 → 目标名已存在（**任何 mode** 的同名项都算已存在）。全部检查在任何写入之前完成。
- 改写 = 源父 tree 去掉该项、目标父 tree 插入**同一 tree oid**（改名只改 `TreeItem.name`），两条父链自底向上重算到根，一次 commit；目标父的项按 Git 顺序排序；源父被移空时补写带时间戳的 `.gitkeep`（同 delete-entry）。
- 落地路径 = 两个父目录的最深公共目录：trunk 上经 `land_api_tip_push` 前进**覆盖该路径的最深非根 path tip**（`resolve_trunk_land_path`，AW-03：落地 `/project/a`（本身无 tip）时前进的是 `/project` 的 tip），因此**跨顶层目录**的移动（公共目录为 `/`）在 trunk 上因 B0 返回 **400**；Review 形态在该公共目录（或 `/`）的 CL 上进行，与 create/delete 相同的政策分流。
- 鉴权：`from_path` 与 `to_path` 各调一次 `trunk_write_requester`，任一失败（401/403）即拒绝，此时尚未读任何 tree。

### 错误映射

| 情况 | 今日实况 / 落地要求 |
|---|---|
| 目录重复名 | **今日 `create-entry` 返回 500** + `err_message:"Internal server error"`。`GitError::CustomError("Duplicate name")` 没有 `[code:]` 前缀（`mono_api_service.rs:2203`、`:2344`），落到 `ApiError::internal`（`common/errors/api.rs`），而 `IntoResponse` 对 5xx 一律改写为 `"Internal server error"`。**LB-02/03 的 delete/move 必须用 `[code:400]` 前缀返回可诊断 4xx**，不得复制这个缺陷 |
| `is_directory=false` 缺 `content` | 同上，今日为 500（`"content is required for file creation"` 亦无 `[code:]` 前缀，`mono_api_service.rs:2121`） |
| tag 名非法 | 今日已是 **400**（`tag_router.rs` 的 `validate_tag_name` → `ApiError::bad_request`） |
| tag 已存在 | **400**（`"[code:400] Tag '{}' already exists"`，`mono_api_service.rs:1677`/`:1690`） |
| tag 不存在（**get**） | **404**；wire `err_message` = `Tag '<name>' not found`。由 router 直接构造（`tag_router.rs:288-291`），服务层 `get_tag` 只返回 `Ok(None)`，**不**产生 `[code:]` |
| tag 不存在（**delete**） | **404**；服务层抛 `"[code:404] Tag not found"`（`mono_api_service.rs:1846`），wire `err_message` = `Tag not found`（前缀被剥掉，见下） |
| delete-entry：目标是文件 | **400**，wire `err_message` = `'<name>' is not a directory`（服务端 `[code:400]` 前缀被剥掉） |
| delete-entry：缺父目录 / 缺目标 | **404**，`parent directory <path> not found` / `entry '<name>' not found under <path>` |
| delete-entry：父路径穿过一个文件 | **400**，`parent path <path> is not a directory` |
| delete-entry：`path`/`name` 不合规（未 rooted、`.`/`..`/空组件、分隔符、控制字符） | **400**，`validate_entry_target` 的诊断原文 |
| delete-entry：ImportRepo（`import_dir` 下） | **409**，`import dir does not support delete entry` |
| move-entry：源目标相同 | **400**，`source and destination are the same: <path>` |
| move-entry：目标名已存在（任何 mode） | **400**，`'<to_name>' already exists under <to_path>` |
| move-entry：移进自己的子树 | **400**，`cannot move <src> into its own subtree <to_path>` |
| move-entry：源是文件 | **400**，`'<from_name>' is not a directory` |
| move-entry：`from_path`/`from_name`/`to_path`/`to_name` 不合规 | **400**，`validate_entry_target` 的诊断原文 |
| move-entry：源父 / 目标父不存在；源不存在 | **404**，`source parent <path> not found` / `destination parent <path> not found` / `entry '<name>' not found under <path>` |
| move-entry：源父 / 目标父路径穿过文件 | **400**，`source parent path <path> is not a directory` / `destination parent path <path> is not a directory` |
| move-entry：源或目标在 ImportRepo 下 | **409**，`import dir does not support move entry` |
| 缺目录（delete / move 均已按此落地） | `404`（ADR-LB-03）。注意这**不是** `GET /tree` / create-entry 的今日行为：今日 `GET /tree` 对不存在的 path 返回 **200 + `tree_items: []`**（`search_tree_by_path` 取不到时返回 `Ok(None)`，`get_tree_info` 再把它映射成 `Ok(vec![])`——`tree_ops.rs:93`/`:98`/`:256`），而 `create-entry` 会**自动补建**缺失的父层级而不是 404 |
| 鉴权失败 | 见「鉴权」 |

> `[code:NNN]` 前缀是本仓真正的 4xx 约定（`mono_api_service.rs:1097`/`:1677`/`:1690`/`:1846` 都在用）。缺前缀的 `CustomError` 会静默变成 500 并丢失原文。
>
> **该前缀只是服务端内部标记，不会出现在 wire 上。** `IntoResponse`（`common/errors/api.rs:85-89`）与 `map_ceres_error`（`:198-199`）都会在写入 `err_message` 前把它剥掉。所以上表引号里的服务端字面量与调用方实际收到的 `err_message` 不同：例如 tag 重名的 wire 值是 `Tag 'foo' already exists`，**不含** `[code:400]`。调用方**不要**按该前缀做字符串匹配。

## 标签（monorepo tags）

服务层 `MonoApiService` 的 `create_tag` / `list_tags` / `get_tag` / `delete_tag` 已实现并挂在 Review；`storage_only_routers_with`（`src/api/api_router.rs`）**没有** merge `tag_router`，所以 storage-only 上四条路由今日全部 **404**。LB-04 负责挂载并补 trunk 写鉴权。

Git 客户端 push tag 仍然**禁止**（见 [`../monorepo.md`](../monorepo.md)）；tag 只能走本节 HTTP。

### `POST /api/v1/tags` — create — `specified-unimplemented`（LB-04）

`CreateTagRequest`：

| 字段 | 规则 |
|---|---|
| `name` | **必填**。服务端校验（`validate_tag_name`，`tag_router.rs:93-138`，违反即 **400**）：非空；`name.len() <= 255`（**字节**，非字符）；不含 `..`；不含 `@{`；不含 `//`；不以 `.lock` 结尾；不含禁用字符 —— **ASCII 空格**、`~`、`^`、`:`、`?`、`*`、`[`、`\`（`tag_router.rs:122` 的 `forbidden` 数组，逐字为 `[' ', '~', '^', ':', '?', '*', '[', '\\']`）；不含 NUL 与任何 `char::is_control()` 字符 |
| `target` | 可选；serde alias `target_commit` |
| `path_context` | 可选；省略则 handler 用 `/`（**也是 trunk 鉴权 path**） |
| `tagger_name` / `tagger_email` / `message` | 可选 |

> **无**请求键 `tagger`——`tagger` 只是 `TagResponse` 的响应字符串。

成功：**200** + `CommonResult<TagResponse>`。

> **现网 OpenAPI 误写 201。** handler 返回 `Json<CommonResult<TagResponse>>`（`tag_router.rs:156`），axum 序列化为 **200**，所以**权威状态是 200**；utoipa 注解在 `tag_router.rs:149` 写的是 201。LB-04 必须把注解改成 200 并加回归，不得让 201 与 200 并存。

### `POST /api/v1/tags/list` — list — `specified-unimplemented`（LB-04）

**方法是 POST，不是 GET。** body 为 `PageParams<String>`：

| 键 | 规则 |
|---|---|
| `pagination` | **必填**，`{ page: u64, per_page: u64 }`，两个子键也都必填。`page` 从 **1** 起（内部 `page.saturating_sub(1)`，故 `page:0` 等同 `page:1`）；`per_page` **必须 ≥ 1**：`per_page = 0` 会让 `mono_storage.rs:1792` 的 `.paginate(self.get_connection(), page.per_page)` 触发 sea-orm 的 `assert!(page_size != 0)`（`sea-orm-2.0.2/src/executor/paginator.rs:318`）而 **panic**；本仓**没有** `CatchPanicLayer`，调用方看到的是连接中断，既不是 4xx 也不是 5xx。`mono_api_service.rs:1764-1768` 里的 `0 → 20` 回退**只**作用于 lightweight ref 的补页（`:1769-1774`），在 DB 分页已经 panic 之后才会走到，救不了这个输入 |
| `additional` | **必填**，这里是 path context |

两个键都**没有** `#[serde(default)]`（`src/contract/api/common.rs:52-56`），因此**都必须出现在 JSON 里**。server 把 `additional.trim().is_empty()` 视同 `/`（含纯空白），但调用方列 root 应显式送 `additional: "/"`。

成功：**200** + `CommonResult<TagListResponse>`，其中 `TagListResponse = CommonPage<TagResponse>` = `{ "total": <u64>, "items": [ TagResponse… ] }`。

> `total` = DB 里的注解 tag 总数 **加上**本次请求扫描到、且已扣除与本页 annotated 重名后的**全部** lightweight ref 数（`mono_api_service.rs:1763`）。注意该加数在 `.take(need)` **之前**就已算出（`:1769-1774`），所以其中可能包含**并未进入本页 `items`** 的 refs。因此 `total` **随页而变**，不是稳定的全局计数；分页请以 `items` 长度与 `per_page` 判断，不要把 `total` 当权威总量。

### `GET /api/v1/tags/{name}` / `DELETE /api/v1/tags/{name}` — get / delete — `specified-unimplemented`（LB-04）

两者 handler **硬编码** `repo_path = "/"`（`tag_router.rs:265`、`:309`）：**MVP 只操作 monorepo root tags**。

调用方**不得**假设存在 path 级 get/delete；路径级语义见计划的 `DEFER-LB-03`。

- get 成功：**200** + `CommonResult<TagResponse>`；tag 不存在 → **404**。
- delete 成功：**200** + `CommonResult<DeleteTagResponse>`（`{ deleted_tag, message }`）；tag 不存在 → **404**。

`TagResponse` 字段：`name`、`tag_id`、`object_id`、`object_type`、`tagger`、`message`、`created_at`。七个字段**全部是非 `Option` 的 `String`**（`ceres/model/tag.rs:37-52`），键始终存在；`created_at` 是**字符串**不是数值时间戳；lightweight tag 的 `tagger` 与 `message` 为**空串**（`mono_api_service.rs:1755-1756`）。

## 鉴权

目录变更写复用既有 `authorize_trunk_api_write`（`src/api/api_write_auth.rs`），与 LFS / create-entry 同一威胁模型。

> **整节的形态前提：** 这道闸**仅**在 `push_policy=trunk`（含 storage-only）时生效。Review 形态下 `trunk_write_requester` 返回 `Ok(None)` 并**跳过**鉴权（`preview_router.rs:470-480`），走既有 CL 分支。

### 不需要 Authorization

`GET /tree`、`POST /tags/list`、`GET /tags/{name}` —— 调用方**不送**、server **不要求** Authorization。

### 需要 Authorization（trunk 写）

| 路由 | 状态 |
|---|---|
| `POST /create-entry` | `implemented`——今日确实鉴权（`preview_router.rs:114-117` 取 `HeaderMap` 并调 `trunk_write_requester`） |
| `POST /delete-entry` | `implemented`（LB-02）——`delete_entry`（`preview_router.rs:138`）取 `HeaderMap` 并调 `trunk_write_requester(path = 父目录)`，鉴权先于任何存储访问 |
| `POST /move-entry` | `implemented`（LB-03）——`move_entry`（`preview_router.rs:166`）对 `from_path` 与 `to_path` 各调一次 `trunk_write_requester`，任一失败即拒绝，先于任何存储访问 |
| `POST /tags` / `DELETE /tags/{name}` | `specified-unimplemented`（LB-04） |

> **今日状态（安全相关，务必读）：** `create_tag`（`tag_router.rs:153`）与 `delete_tag`（`tag_router.rs:305`）**都不接收 `HeaderMap`，也不调用任何 authorizer**——`tag_router.rs` 全文没有 `authorize_trunk_api_write`。因此今日 tag 写在 **Review 面上完全匿名**（cedar_guard 只覆盖 `/cl`，不覆盖 `/tags`），在 **storage-only 面上 404**。这正是计划里的 `GAP-LB-04`。下面描述的是 **LB-04 落地后**的契约，不是今日行为。

落地后（LB-04）：

| `push_auth` | 行为 |
|---|---|
| `token` | Bearer，或 Basic 的密码栏 |
| `none` | 无 header 即可写，requester 记为 `anonymous` |

状态码分类（`api_write_auth.rs:33-48`）：

- 凭据**缺失**，**或**提供了但查不到对应 push token → **401**（两者都走 `api_write_auth_challenge`）
- 凭据识别成功、但 `paths` 未覆盖目标 path → **403**
- `push_auth` **未配置**（`None`）→ **401**，fail-closed

错误响应、JSON 与 trace **不得**回显 token。

### tag 写需要能覆盖 `/` 的 token（LB-04 落地后）

> 本小节同样描述 **LB-04 落地后**的行为。今日 tag 写在 storage-only 上 404、在 Review 上完全匿名（见上方「今日状态」）。

tag 写的鉴权 path 不是你操作的业务路径：

- **create** 用 `path_context.as_deref().unwrap_or("/")`（`tag_router.rs:172`）；
- **delete 无 body，LB-04 的授权 path 固定 `/`**（今日 `tag_router.rs:309` 的 `repo_path = "/"` 只是 handler 分发用，不是鉴权）。

因此 `push_tokens.paths = ["/project"]` 这类**不含 `/`** 的 token：

- 对 **每一次** tag delete → **403**；
- 对**省略**或显式 `path_context = "/"` 的 create → **403**。

调用方（含 Libra）要做 tag 写，必须持有能覆盖 `/` 的 token（`paths` 省略或为空 = whole repo）。

> 契约页不写真实凭据；示例一律用 `secret-ok` 一类占位。

## 形态差异

| 形态 | 目录变更落地 | `cl_link` |
|---|---|---|
| trunk / storage-only | `land_api_tip_push` | 必为 `null` |
| Review | 既有 `find_or_create_cl_for_edit` CL 分支 | 可为非 null |

`write_routers`（`preview_router.rs:57-63`）同时挂在 Review 与 trunk；LB-02 / LB-03 已把 `delete-entry` 与 `move-entry` 登记进同一函数，因此两者在两种形态下都可用（storage-only OpenAPI 由 `server::http_server::tests::storage_only_openapi_*` 锁定）。

`ImportRepo`（`import_dir` 下）对 delete/move 必须返回 **409**（计划门 `delete_entry_reject_import_repo_409`，见 plan-20260917 LB-02 卡的 Verification；该门只规定状态码）。LB-02 的 `delete_monorepo_entry`（`import_api_service.rs`）返回 `[code:409] import dir does not support delete entry`，wire 上 `err_message` = `import dir does not support delete entry`；LB-03 的 move 同样：源在 ImportRepo 下由 `ImportApiService` 返回 `[code:409] import dir does not support move entry`，目标在 ImportRepo 下由 monorepo handler 自查 `git_repo` 后返回同一文本。实现上**必须**带 `[code:409]` 前缀——这是推导出来的必要条件，不是计划原文：`GitError::CustomError` 只有带该前缀才会被 `common/errors/api.rs:175` 映射成 `StatusCode::CONFLICT`。**另有一个陷阱：** `map_ceres_error`（`common/errors/api.rs:194-208`）只识别 `400` 与 `404`，其余一律 `_ => ApiError::internal` → **500**，而它正是 `tag_router.rs` 的惯用写法。因此 delete/move 必须走裸 `?`（`From<E> for ApiError`，如 `preview_router.rs:121`）那条路径；若用 `map_ceres_error` 包装，即使带了 `[code:409]` 仍会落成 500 并使该门失败。今日 create-entry 的同类拒绝（`import_api_service.rs:56-64` 的 `CustomError("import dir does not support create entry")`）**没有**前缀，因而落成 500 + `"Internal server error"`——见「错误映射」，delete/move 不得复制该缺陷。其 Git 多分支与客户端 tag 语义不受本计划影响。

## Git 可见性

目录变更成功后，对同一 path tip 的 `git clone` / `git fetch` + `git pull` 必须看到删除结果或新路径。

tag create/delete 之后，`GET /tags/{name}` 与 `POST /tags/list` 必须一致。

## 非目标

- 不删除或移动**文件**；不预览/编辑 blob；不建文件（`is_directory=false` 保持既有 create-entry）。
- 不改 `GET /tree` / `POST /create-entry` / `POST /edit/save` 的既有请求字段或成功语义。
- 不以 Git receive-pack delete command、parent-path 客户端 push 或 CL `apply_changes` 冒充产品 HTTP。
- 不把 list tags 从 `POST /tags/list` 改成 `GET`（只改文档对齐程式，**不改 wire**）。
- 不把 `get_tag` / `delete_tag` 的 root-only 扩成任意 path（只文件化；见 `DEFER-LB-03`）。
- 不接入 Review OAuth / Cedar enforce；不为 storage-only 打开 SSH receive-pack。
- 不实现 Libra 客户端（`DEP-LB-03`）。

## 相关文档

- [`../monorepo.md`](../monorepo.md) —— 产品规则；tag 只能走 HTTP，Git 客户端禁 tag。**注意：** 该文第 52 行的 list 仍写 `GET`，属已知文档缺陷，由 LB-04 对齐；wire 以本页为准
- [`../deploy-trunk.md`](../deploy-trunk.md) —— storage-only 运维手册与产品 API 写契约
- [`../plan/plan-20260904.md`](../plan/plan-20260904.md) —— create-entry / edit/save + `push_auth` + `land_api_tip_push` 的来源计划
- [`integration.md`](integration.md) —— 集成测试与黑盒矩阵
