# 使用指南

[English](user-guide.md) · 中文

mega2 是面向 Agent 的第二代 Mega 引擎，主要提供 Monorepo 引擎与可选的 Agent Session Capture。推荐的 Agent 工作流会将 mega2 与 ScorpioFS（本地文件系统挂载）和 Libra（版本控制工作流）配合使用。本指南介绍日常 Git、HTTP API、大文件、可选服务和 CLI 操作；部署步骤见[部署指南](./deployment.zh.md)，配置键以带注释的 [`config.toml`](../config/config.toml) 为准，错误类型与状态码见[错误模型](./errors.md)。

> **范围：**mega2 开源版只支持 **trunk / storage-only** 部署模式，不提供 Web UI 或 Change List（CL）。交互式浏览和目录、Tag 操作请使用 Libra 的 `libra mega2 browser`（见第 6 节）。

## 1. 产品形态与边界

仓库行为取决于你使用的路径：

- **Monorepo 路径**（根路径及 `import_dir` 之外的路径）只公开 `main` 分支。Git 客户端推送其它分支或 Tag 都会被拒绝。请通过 HTTP API（见第 4.2 节）或 Libra 命令 `libra mega2 browser`（见第 6 节）创建、查看和删除 Tag。
- **ImportRepo 路径**位于 `[monorepo].import_dir`（默认 `/third-party`）下，遵循常规 Git 语义，允许多个分支和 Git 客户端 Tag 操作，可以持续推送更新，也可以清理（见第 2.6 节）。迁移或托管第三方仓库时可使用这类路径。
- 随仓库提供的 storage-only 服务不提供 Web UI，也不包含 Change List（CL）。其 OpenAPI 文档不包含 CL、issue、reviewer 和 user 路由。
- Monorepo 路径上的 Git push 与产品 API 写入共用写入队列和路径 tip 权威；存活 ImportRepo 内部的编辑保存直接写该仓库，见第 4.1 节。写入流程见[架构设计](./architecture.zh.md)。

## 2. Git 客户端操作

### 2.1 Smart HTTP:clone / fetch / push

Git Smart HTTP（`info/refs`、`git-upload-pack`、`git-receive-pack`）挂载在仓库路径下，支持克隆子路径。本地 Compose 栈的端口见 [`deploy-trunk.md`](./deploy-trunk.md) 第 8 节：

```bash
git clone http://127.0.0.1:9000/project
git fetch && git reset --hard origin/main   # trunk 推送后的客户端对齐动作
```

trunk 模式下的推送规则（Monorepo 路径；ImportRepo 见第 2.6 节）：

- 只推送到 `refs/heads/main`；Monorepo 路径上其它分支和 Git 客户端 Tag 写入都会被拒绝。
- 单 commit 推送会原样写入。包含多个 commit 的推送会由服务端 squash 成一个 commit 写入 `main`。推送后运行 `git fetch && git reset --hard origin/main` 与远端对齐，否则下次 push 可能因 non-fast-forward 被拒绝。
- 推送到仓库子路径；storage-only 模式不支持从根路径 `/` 推送。

协议兼容性与 smoke 矩阵见 [`refactoring/protocol.md`](./refactoring/protocol.md)。

### 2.2 SSH:只读 fetch

storage-only **不暴露 SSH receive-pack**(`git.ssh_receive_pack=false` 是强制配置,省略会拒绝启动);SSH 仅用于 clone / fetch / pull。`anonymous_access` 与 `push_auth` 组合下的认证形态见 [`deploy-trunk.md`](./deploy-trunk.md) 第 4 节。

### 2.3 推送鉴权:token 或 none

写面(Git receive-pack、LFS 批/锁写、产品 API 写)共用 `git.push_auth`:

- `token`:HTTP Basic 只取 password(= token 密文),忽略 username;`paths` 前缀按组件边界授权。
- `none`:无凭据写,仅限受控网络(回环 / Unix socket / 内网前置);ImportRepo 的 HTTP 清理端点例外,`none` 时对合法请求一律回 403(见第 2.6 节)。

`[[git.push_tokens]]` 配置、凭据注入方式与风险提示见 [`deploy-trunk.md`](./deploy-trunk.md) 第 3 节;本文不复制 token 值。

### 2.4 对象格式

`[monorepo].object_format` 默认 `sha1`(标准 Git)。`sha256` 与 `blake3` 是 **git-internal / Libra 扩展**,不宣称与标准 Git 客户端互通,需配合 Libra 使用;口径见 [`refactoring/protocol.md`](./refactoring/protocol.md) 与 [`refactoring/config.md`](./refactoring/config.md)。

### 2.5 Monorepo 路径策略与首次使用

本节是「在 Monorepo 中建一个新路径」的唯一权威说明；快速开始、README 与初始化手册都链接到这里。

**允许的根。**`[monorepo].root_dirs` 列出的一级目录（取值见 [`config/config.toml`](../config/config.toml)，形状规则见[配置指南](./configuration.zh.md)）是「创建」白名单：新路径只能建在某个根之下。`import_dir`（默认 `/third-party`）之下的路径是 ImportRepo（见第 1 节），由 Git 推送直接创建，不适用本节，其更新与清理见第 2.6 节。新增一级根要改配置并重启服务；此后该根还不在根树中，只有在已有历史之上新增 commit 的推送能创建它——开通与产品写都不在 `/` 落地。

**第一次推送到新路径。**目标路径已经存在时（已被推送、开通或 API 写入过），按第 2.1 节的规则推送即可。目标路径尚不存在时，trunk 模式按下表处理；表中两种路径策略拒绝在原样重试时保持不变：

| 情形 | 结果 | 下一步 |
|---|---|---|
| 路径不在任何允许的根之下 | 拒绝，`MONO_PATH_NOT_ALLOWED`，列出允许的根 | 改用某个根之下的路径；需要新根时见上文「允许的根」 |
| 路径在允许的根之下，推送的历史从零开始（例如 `git init` 后的第一个 commit），或把另一路径的历史原样推过来 | 拒绝，`MONO_PATH_UNINITIALIZED`，点名开通命令 | 按下文开通路径，clone 后在其上提交，再推送；尚不在根树中的新根见上文「允许的根」 |
| 路径在允许的根之下，推送在服务端已有的 commit 之上至少新增一个 commit（例如 clone 另一个路径后提交） | 接受，按创建语义落地 | 推送后运行 `git fetch && git reset --hard origin/main` 对齐 |

第二、三种情形下新路径不广告任何 ref，Git 会把整段源历史一起发送：其中的 commit 超过 `[monorepo].max_push_commits`（默认 250）时推送以该上限被拒（不带路径策略码，原样重试时文本可能不同）。此时对已有的根改用开通；初始化后新增的根无法开通，可 clone 一个历史较短的路径，在其上提交后推送到新根。

**开通路径。**开通只创建目录（写入一个 `.gitkeep` commit），可重复执行：

- CLI：`mega2 path provision --server <URL> <PATH>`。只读取环境变量 `MEGA2_TOKEN` 作为 push token（`push_auth=none` 时可省略），不读配置文件；成功输出 `created <path> (<commit>)` 或 `already exists <path>` 并以 0 退出，服务端拒绝时把错误文本写到 stderr 并以 1 退出。
- HTTP：`POST /api/v1/path/provision`，请求体 `{"path": "<PATH>"}`；鉴权、状态码与响应形态见运行时 OpenAPI（第 4.4 节）与契约页 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)。

开通之后 `git clone <URL><PATH>`，在其上提交并推送。第 4.1 节的产品写在已位于根树中的允许根之下本身就会创建路径，不需要先开通。

**错误码。**路径策略错误的文本以稳定的码开头，客户端可按冒号前的码分支；完整文本格式、HTTP 状态与各码的出现场景见[错误模型](./errors.md)。四个码与对应动作：

- `MONO_PATH_NOT_ALLOWED`：改用允许的根之下的路径（开通，以及没有存活 ImportRepo 处的产品写，对 `import_dir` 之下的路径也返回此码）。
- `MONO_PATH_UNINITIALIZED`：先开通该路径。
- `MONO_PATH_INVALID`：改用规范路径（绝对路径，不含 `.` / `..`、重复或末尾的 `/`）。
- `MONO_PATH_CONFLICT`：路径上的某一级已是文件，换一个路径（只由开通返回，API 与 CLI 均然）。

### 2.6 ImportRepo 生命周期

本节是 ImportRepo 更新、清理与保留策略的权威说明，运维 CLI `mega2 import-repo remove` 的用法也以本节为准。ImportRepo 位于 `[monorepo].import_dir`（默认 `/third-party`）之下，`import_dir` 如何划分路径见[初始化手册](./manual/monorepo-init.zh.md)。

**创建与后续推送。**推送到 `import_dir` 之下尚不存在的路径会创建 ImportRepo，并把它挂载到 Monorepo 根树中；路径必须严格位于 `import_dir` 之下：推送到 `import_dir` 本身时服务端以 400 `IMPORT_REPO_PATH_INVALID` 拒绝，Git 客户端只显示 `The requested URL returned error: 400`。此后它是一个可以持续更新的普通 Git 仓库：分支与 Tag 的新建、更新、删除，以及 `--force` 的非快进更新都会被接受。后续推送只更新 ref，不在 Monorepo 根树上产生新 commit，因此第 2.1 节的 squash 与 `git fetch && git reset --hard origin/main` 对齐步骤不适用于 ImportRepo。

**并发与失败粒度。**推送中每个 ref 的更新都以服务端广告时的值为前提：广告之后其它推送或 API 写先改动了该 ref 时，本次对它的更新以 `IMPORT_REPO_STALE_REF` 拒绝（更常见的是 Git 客户端在发送前就以 `fetch first` 拒绝）：先从该 ImportRepo 取得最新 ref（例如 `git fetch mega2`），合并或变基后再推送，有意覆盖时使用 `--force`；Tag 冲突需要单独处理。产品 API 的 `edit/save` 直接写默认分支、不做这一校验，可能覆盖同时发生的推送（第 4.1 节）。同一次推送中，Tag 逐个写入、各自成败；全部分支作为一个批次写入，批次失败时每个分支都报告同一个原因，没有分支前进。推送报告总以 `unpack ok` 开头；服务端不支持 `git push --atomic`。各错误码的完整文本、HTTP 状态与出现场景见[错误模型](./errors.md)的「ImportRepoError」一节。

**嵌套 ImportRepo。**先推送父路径、再推送其下的子路径是可以的，之后父仓库仍可更新（父仓库是升级前挂载、没有挂载来源记录的存量仓库时除外，见下文「兼容性」）；反过来，已有子仓库时再推送父路径，其分支会以 `IMPORT_REPO_PATH_OCCUPIED` 拒绝。清理时须先清理子仓库，否则以 `IMPORT_REPO_HAS_CHILDREN` 拒绝。

**清理。**清理把 ImportRepo 从服务中摘除：删除仓库记录与 ref，从 Monorepo 根树中移除属于该仓库的挂载点（产生一个 `Remove ImportRepo <P>` 根 commit；不属于它的挂载内容保持不动，见下文「兼容性」），随后分批清扫该仓库的 commit、tree、blob 与 tag 元数据。摘除之后，该路径的 clone / fetch 回 404。摘除提交时仍在进行中的推送，此后要写的 ref 以 `IMPORT_REPO_REMOVED` 拒绝，其中的分支不会落地（Tag 逐个写入，摘除之前已写入的 Tag 属于旧仓库，随清理一并删除）；摘除之后才开始的推送——即使清扫尚未完成——会把该路径导入为一个新的仓库。清理有两个入口，使用同一套清理机制：

- HTTP `POST /api/v1/import-repo/remove`（第 4.6 节）：要求 `git.push_auth = "token"`，且 token 覆盖该路径（第 2.3 节）；`push_auth = "none"` 的部署对合法的清理请求一律回 403（请求体或路径不合法时先回 400 / 422）。请求字段、结局（`removed` / `pending` / `absent`）、续做协议与错误码见契约页 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) 的「ImportRepo 叶子清理」。
- 运维 CLI `mega2 import-repo remove`：在服务器上以服务自己的配置运行，适用于 `push_auth = "none"` 等无法使用 HTTP 清理的部署；能读取服务配置及其中的数据库凭据，即是授权。

两个入口的 `absent` 都表示该路径已没有未完成的清理，CLI 的 `removed` 同样如此（运行结束前会排空该路径其余的清理）；HTTP 的 `removed` 只保证本次请求所指的清理已完成，同一路径更早的未完成清理可能仍在，由后续请求继续（见契约页）。

**运维 CLI。**在 Compose 栈中进入 mega2 容器运行（镜像已设置 `MEGA_CONFIG`；`-f` / `-p` 与启动该栈时相同）。裸机部署时使用与服务相同的配置文件、`--profile` / `MEGA_PROFILE` 与 `MEGA_*` 环境变量（例如经环境变量注入的数据库地址），且 `--config` 必须放在 `import-repo` 之前：

```bash
docker compose -f <compose 文件> [-p <项目名>] exec -T -e MEGA_LOG__PRINT_STD=false mega2 mega2 import-repo remove --path /third-party/<repo> --yes
mega2 --config /etc/mega2/config.toml import-repo remove --path /third-party/<repo> --yes
```

- 前提：需要 0.40.11 及以上；服务正在运行（中断遗留的写入队列行由服务回收）；使用与服务相同的镜像或二进制——二进制与数据库 schema 不一致时拒绝执行，命令从不迁移数据库。只接受 storage-only 部署（`git.push_auth` 为 `token` 或 `none`）。配置必须经 `--config` 或 `MEGA_CONFIG` 点名，不会生成默认配置。本次运行的 `redis.url` 与 S3 凭据必须是字面值（`vault://` 引用会被拒绝，可用 `MEGA_REDIS__URL`、`MEGA_OBJECT_STORAGE__S3__ACCESS_KEY_ID` 与 `MEGA_OBJECT_STORAGE__S3__SECRET_ACCESS_KEY` 覆盖）；命令不打开 vault。不要为本次运行设置 `MEGA_ID_GENERATOR_WORKER_ID`：设置后 CLI 不再从 Redis 领取 worker 租约，可能与服务使用同一个 worker id。缺少 `--yes` 时不执行任何操作。
- 输出：stdout 的最后一行是结局行，路径中的控制字符会被转义；日志打到 stdout 时（`MEGA_LOG__PRINT_STD=true`），此前各行是日志。退出码 1 与 2 时不输出结局行，原因写在 stderr；退出码 3 与 130 时 stdout 有结局行，stderr 给出说明。
- Ctrl-C：下表的 130 适用于在前台直接运行 CLI 的情形。在 `docker compose exec -T` 下按 Ctrl-C 只会结束 compose 客户端（它以 130 退出、没有结局行），容器内的命令会继续运行到结束：不要立即重跑，按下文确认它已结束后再查询台账。

| 结局行 / 情形 | 退出码 | 含义与下一步 |
|---|---|---|
| `removed <P> (repo_id=R, cleanup_id=C)` | 0 | 已摘除并清扫完毕。路径上没有存活仓库、只剩未完成的清理时，R 与 C 可能指向更早被摘除的仓库 |
| `absent <P>` | 0 | 这个路径上既没有存活仓库，也没有未完成的清理。`--path` 按原样精确匹配存储的仓库路径，不会去掉 `.git`（Git URL 会），也不纠正拼写：执行前请用下文「查询台账」的 `psql` 形式查询 `SELECT id, repo_path FROM git_repo WHERE repo_path = '<P>'`，确认路径与仓库 id |
| `pending <P> (repo_id=R, cleanup_id=C)` | 3 | 已摘除，清扫未完成（达到 `--max-rounds`，默认 1000 轮，或摘除之后失败）；用 `--cleanup-id C` 续做 |
| `interrupted <P> (repo_id=R, cleanup_id=C)` | 130 | 被 Ctrl-C 中断，摘除已提交；用 `--cleanup-id C` 续做 |
| `interrupted <P>` | 130 | 被 Ctrl-C 中断且没有记录清理，仓库不变，重跑同一命令；stderr 说明台账无法读取时见下文 |
| stderr 给出错误（例如 `<CODE>: …`、`import-repo remove: …`、`Other error: …`、`Database error: …`） | 1 | 本次没有记录清理：按错误码处理（例如 `IMPORT_REPO_HAS_CHILDREN` 须先清理子仓库），或修正前提后重跑；台账无法读取时见下文 |
| 缺少 `--yes` 等用法错误 | 2 | 未执行任何操作 |

- 续做：`--cleanup-id <C>`（配合同一个 `--path`）只继续清扫台账中的这一行，从不摘除存活的仓库，即使该路径已被重新推送也是安全的；不带 `--cleanup-id` 重跑，则会摘除当时存活的仓库（包括期间重新导入的仓库）。主清理完成之后，运行还会按台账清扫该路径其余未完成的清理（`--cleanup-id` 时可能包括更新的行），期间同样不摘除任何仓库；以 `pending` 结束时，其余的行可能尚未处理。
- 台账无法读取：失败或中断之后，CLI 读取清理台账来给出续做句柄；stderr 说明台账无法读取时（退出码 1，或 130 且结局行不带 id），摘除可能已经提交，先按下一条查台账，再决定续做或重跑。
- 查询台账：进程被杀死（SIGTERM、SIGHUP 与 SIGKILL 均不处理）、崩溃，或在 `exec -T` 下按了 Ctrl-C 时，先确认命令已结束（Compose 栈可用 `docker compose -f <compose 文件> [-p <项目名>] top mega2` 查看容器内是否还有 `mega2 import-repo remove`），再查询该路径的清理台账（评估栈的库用户与库名都是 `mega2`，其它部署使用服务配置中的数据库）：

  ```bash
  docker compose -f <compose 文件> [-p <项目名>] exec -T postgres psql -U mega2 -d mega2 -c "SELECT id, repo_id, state FROM import_repo_cleanups WHERE path = '/third-party/<repo>' ORDER BY id"
  ```

  `state` 为 `detached` 的行用 `--cleanup-id <id>` 续做；没有 `detached` 行表示没有未完成的清理。此时该路径若仍能 clone，它可能是原来的仓库（摘除没有提交），也可能是期间重新导入的新仓库；先核对（例如看最近的提交）再决定是否重跑，重跑会摘除当时存活的那个仓库。

**保留策略。**清理删除的是元数据：仓库记录与 ref、Monorepo 根树中属于该仓库的挂载点，以及该仓库的 commit、tree、blob 与 tag 行（后者按轮分批清扫，大仓库可能需要多轮）。对象存储中的 blob 字节会保留，服务端没有垃圾回收。清理台账 `import_repo_cleanups` 与审计日志（`kind = import_repo.remove`）永久保留，并记录请求者：HTTP 为 token 名，CLI 为 `operator-cli`。清理不可撤销，再次推送同一路径会导入为一个新仓库（新的 `repo_id`）。已知限制见契约页「ImportRepo 叶子清理」一节。

**兼容性。**存量 ImportRepo 无需数据迁移即可接受后续推送：挂载点只含 `.gitkeep` 且至少有一个分支的存量挂载，会在下一次新建或更新分支的推送时自动补齐挂载来源记录。另有两种存量形态，新建或更新分支的推送会以 `IMPORT_REPO_PATH_OCCUPIED` 拒绝（Tag 推送与只删除分支的推送不受影响）：

- 升级前挂载、其下已有嵌套子仓库的父仓库，按顺序处理：①备份（`git clone --mirror`）并清理子仓库；②向父仓库推送一次新建或更新分支，补齐它的挂载来源记录（没有新内容时可推送一个临时分支再删除）；③之后再重新推送子仓库。在第 ② 步之前推回子仓库，父仓库会再次被拒。
- 没有任何分支的存量挂载：清理只删除它的记录，根树中的占位目录保留，该路径之后仍无法导入，请改用其它路径。存量挂载在补齐来源记录之前，不要用只删除分支的推送删掉它的全部分支，否则会变成这种形态。

清理台账表由升级时的自动数据库迁移创建；0.40.4 起升级还会规范化存量的 ImportRepo 别名路径，见 [`deploy-trunk.md`](./deploy-trunk.md) 第 9.2 节。

## 3. 大文件:LFS

- **Git LFS(标准)**:端点 `/info/lfs` 与 `/api/v1/lfs`,stock `git-lfs` 客户端直接可用;LFS 写授权与 Git receive-pack 共用 `git.push_auth`(矩阵见 [`deploy-trunk.md`](./deploy-trunk.md) 第 6 节)。

## 4. HTTP API 使用

Base URL 为 HTTP 服务监听地址。写接口经 `git.push_auth` 鉴权,无凭据 / 坏 token 回 401,path 越权回 403(同 Git 写面)。

### 4.1 产品写(目录与文件)

`POST /api/v1/create-entry`、`POST /api/v1/delete-entry`、`POST /api/v1/move-entry` 和 `POST /api/v1/edit/save` 可创建、删除、移动目录与文件，Monorepo 路径上的写入经过与 `git push` 共用的 MonoWriteQueue 和 path tip。存活 ImportRepo 内部的产品写由该仓库自己处理：`edit/save` 直接写默认分支，不经写入队列，也不做推送那样的 ref 校验；删除与移动回 409，新建不受支持（当前回 500）。成功响应中的 `cl_link` 为 `null`（不会创建 CL）；写入提交后，同栈的 `git clone` 或 `git pull` 可立即读到内容。依赖路径索引的浏览结果可能稍后才更新。请求字段与鉴权契约见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)。

### 4.2 Tags API

Monorepo 路径禁止 Git 客户端推送 Tag，Tag 经此入口管理（ImportRepo 也可以直接用 Git 客户端推送 Tag，见第 2.6 节）:

| 操作 | HTTP |
|---|---|
| 创建 | `POST /api/v1/tags` |
| 列表 | `GET /api/v1/tags/list`(GET-only;`POST` 回 405) |
| 查询 | `GET /api/v1/tags/{name}` |
| 删除 | `DELETE /api/v1/tags/{name}` |

创建和删除请求通过 `git.push_auth` 鉴权；列表与查询请求无需 Authorization。请求字段、路径选择器和授权规则见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)。

### 4.3 只读浏览

`GET /api/v1/status`、`GET /api/v1/file/blob/{object_id}`、`GET /api/v1/file/tree` 及 blob / tree / blame preview 读路径保留;读授权跟随 `git.anonymous_access`。完整路径清单以运行时 OpenAPI 为准(见 4.4),本文不复制端点表。

### 4.4 OpenAPI 与 Swagger UI

- 机器可读契约:`GET /api/openapi.json`(storage-only 下如实不含 CL / issue / user 路由)。
- 交互式文档:`/swagger-ui`。

### 4.5 错误契约

错误类型归属与 HTTP 状态映射集中在 `crate::common::errors`,规则见 [`errors.md`](./errors.md)。

### 4.6 ImportRepo 清理

`POST /api/v1/import-repo/remove` 摘除并清扫 `import_dir` 之下的一个 ImportRepo，只在 storage-only 形态挂载。它要求 `git.push_auth = "token"` 且 token 覆盖该路径；`push_auth = "none"` 时对合法请求一律回 403，请改用运维 CLI `mega2 import-repo remove`。清理语义、保留策略与续做见第 2.6 节，请求与响应契约见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) 的「ImportRepo 叶子清理」。

## 5. 可选服务面

两个面均为**双重门控**(storage-only 形态 + 各自开关),缺一则整面不存在(裸 404);在非 storage-only 形态下打开开关会拒绝启动。

- **OCI Distribution `/v2`**:storage-only 且 `[oci].enabled=true` 时挂载,可作容器镜像仓库(`docker login` 复用 push token,无独立 token 服务)。架构与端点事实源见 [`refactoring/oci.md`](./refactoring/oci.md),启用步骤见 [`deploy-trunk.md`](./deploy-trunk.md) 第 10 节。
- **Agent Session Capture `/api/v1/agent-capture`**：可选能力，接收并查询 Agent 会话、事件、checkpoint、transcript 与文件操作记录。仅在 storage-only 且 `[agent_capture].enabled=true`、配置至少一个 `[[agent_capture.ingest_tokens]]` 时挂载；使用独立 ingest token，不复用 Git push token，也不会由 Git push 自动产生会话记录。配置与配额事实源见 [`refactoring/agent-capture.md`](./refactoring/agent-capture.md)。

## 6. 配合 Libra 使用

mega2 开源版不提供 Web UI。Libra 的 `libra mega2 browser` 在终端提供基础远程目录浏览，以及受支持的目录和 Tag 列出、创建、删除等操作；这些操作不替代 Git 客户端的 clone、fetch、push:

```bash
libra mega2 browser
```

该 TUI 逐层浏览远程目录，支持创建、删除、移动、重命名目录；Tag 面板可列出、创建和删除根 Tag，创建和删除需要写入凭据。它读取 Mega2 的远程目录与 Tag 数据，不执行 Git clone、fetch、push。sha256 / blake3 对象格式同样只在 Libra 客户端下可用(见 2.4)。

## 7. CLI 速查

全局参数为 `--config <PATH>`（环境变量 `MEGA_CONFIG`）和 `--profile <NAME>`（环境变量 `MEGA_PROFILE`，加载同目录的 `config.<profile>.toml`）。配置加载顺序、环境变量覆盖、未知字段拒绝与热加载白名单见 [`refactoring/config.md`](./refactoring/config.md)。对于新建数据库，运行 `mega2 service init --yes` 会根据 `root_dirs` 创建初始 Monorepo，然后退出，不会启动监听；目录默认值见带注释的 [`config/config.toml`](../config/config.toml)。

| 命令 | 用途 |
|---|---|
| `mega2 service init --yes` | 初始化空 Monorepo 并退出(不启动监听);空卷 bootstrap 的固定动作 |
| `mega2 service http --host 0.0.0.0 -p 8000` | 启动 HTTP 面(Git smart HTTP + LFS + `/api/v1` + 可选 OCI / Agent Capture);与 Dockerfile 默认 CMD 一致 |
| `mega2 service ssh [--ssh-port 2222]` | 启动 SSH 面(storage-only 下仅 upload-pack);standalone 形态要求 `cedar.enforcement=off`,否则改用 `multi` |
| `mega2 service multi http ssh` | 单进程同时启动 HTTP + SSH(共享授权快照) |
| `mega2 path provision --server <URL> <PATH>` | 开通 Monorepo 路径(首次推送前,见 2.5);token 只从环境变量 `MEGA2_TOKEN` 读取,不读配置文件 |
| `mega2 import-repo remove --path <P> --yes [--cleanup-id <ID>] [--max-rounds <N>]` | 在服务器上清理 ImportRepo(storage-only;`--config` 放在 `import-repo` 之前或设 `MEGA_CONFIG`);结局行与退出码见 2.6 |
| `mega2 config validate [--resolve-secrets] [--show-sources]` | 启动前校验配置;自动化加 `--deny-warnings` |
| `mega2 config init [-o PATH] [--force]` | 生成安全的起始配置文件 |
| `mega2 config secret ref/set/check/rotate` | 管理 vault 承载的配置密文(SecretRef) |
| `mega2 config vault backup/restore/rekey/reset` | vault core key 运维(后三者破坏性,需 `--force`) |
| `mega2 debug storage-smoke [--key ...]` | 隐藏命令:对象存储读 / 写 / 删冒烟,不出现在顶层帮助 |

`authz-audit` 子命令面向 review 形态的授权审计;storage-only 下 `cedar.enforcement` 恒为 `off`,日常用不到(review 形态口径见 [`manual/authz.md`](./manual/authz.md))。

完整 flag 列表见 `--help` 与 `src/commands/`。Compose 启动和 smoke 命令示例见 [`deploy-trunk.md`](./deploy-trunk.md) 第 8 节；本地开发与测试入口见 [`development.md`](./development.md)。

## 8. 相关文档

| 主题 | 文档 |
|---|---|
| 使用与开发指南(快速开始 / 进阶场景 / 配置 / 部署 / 架构 / 贡献) | [`quick-start.zh.md`](./quick-start.zh.md) · [`recipes.zh.md`](./recipes.zh.md) · [`configuration.zh.md`](./configuration.zh.md) · [`deployment.zh.md`](./deployment.zh.md) · [`architecture.zh.md`](./architecture.zh.md) · [`contributing.zh.md`](./contributing.zh.md) |
| 仓库路径、分支、Tag、推送规则与 ImportRepo 生命周期 | 本指南第 1–2 节 |
| 初始化目录布局与配置 | [`manual/monorepo-init.zh.md`](./manual/monorepo-init.zh.md) |
| trunk / storage-only 部署与 Compose 栈 | [`deployment.zh.md`](./deployment.zh.md) · [`deploy-trunk.md`](./deploy-trunk.md) |
| 配置键与环境变量 | [`config/config.toml`](../config/config.toml)(注释样例)、[`refactoring/config.md`](./refactoring/config.md) |
| Git 协议兼容性 | [`refactoring/protocol.md`](./refactoring/protocol.md) |
| trunk 推送设计与 MonoWriteQueue | [`refactoring/trunk-push.md`](./refactoring/trunk-push.md) |
| 目录 / 文件写与 tag 契约 | [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) |
| OCI registry | [`refactoring/oci.md`](./refactoring/oci.md) |
| Agent Capture | [`refactoring/agent-capture.md`](./refactoring/agent-capture.md) |
| 错误模型 | [`errors.md`](./errors.md) |
| 认证与授权边界(review 形态) | [`manual/authz.md`](./manual/authz.md) |
| 本地开发与测试 | [`development.md`](./development.md) · [`contributing.zh.md`](./contributing.zh.md) |
