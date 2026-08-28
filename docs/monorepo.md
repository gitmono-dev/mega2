# Monorepo 产品规则

本文是 monoengine **Monorepo（非 `import_dir` 下的 ImportRepo）** 的集中规则事实源。协议实现、CI smoke、Web/API 与测试矩阵凡涉及分支、标签或仓库初始化，以本文为准；细节实现锚点见 [`refactoring/protocol.md`](./refactoring/protocol.md) 与 `config/config.toml` 的 `[monorepo]`。

> **范围**：默认路径下的 MonoRepo（根路径 `/` 及非 `import_dir` 子树）。  
> **例外**：`[monorepo].import_dir`（默认 `/third-party`）下的 **ImportRepo** 仍可按普通 Git 多分支 / 客户端 tag 语义工作；本文规则不覆盖 ImportRepo。

---

## 1. 公开分支：仅 `main`

### 规则

1. MonoRepo **只有一个公开分支**：`refs/heads/main`（简称 `main`）。
2. 客户端对 monoengine 的 Git 推送 **不得** 在远端创建第二个公开分支（例如 `refs/heads/feature` 不得作为持久 heads 出现）。
3. `git ls-remote` / upload-pack 广告中，heads 侧对用户可见的稳定公开 tip 是 `main`；变更评审用的 tip 落在 `refs/cl/*`，不是公开分支。

### 与 Change List（CL）的关系

- 客户端常见写法 `git push origin HEAD:refs/heads/<name>`（`<name> ≠` 持久公开分支）在 MonoRepo 上会进入 **CL 管线**：服务端落地为 `refs/cl/<id>`，**不会**把 `<name>` 登记为第二个 `refs/heads/*` 公开分支，也 **不会** 直接改写 `main`。
- 合并进 `main`、关闭/更新 CL 等产品动作走 Web / 内部 API，而不是「再 push 一个公开分支」。
- 因此：smoke / 集成测试里的「branch push」验收的是 **CL 创建与定向 fetch**，不是多分支托管。

### 测试与 CI 期望

| 场景 | 期望 |
|---|---|
| 推送非 `main` 的 heads 命令 | 成功时只新增 `refs/cl/*`；`refs/heads/<smoke-name>` **不** 出现在 `ls-remote` |
| 推送后 `ls-remote refs/heads/*` | 仍以 `main` 为公开 heads（无新增公开分支名） |
| 对新建 `refs/cl/*` 的 fetch/checkout/pull | 工作树与 push 前本地树一致（见 `protocol.md` / `integration_git_cli`） |

权威自动化：`scripts/git_protocol_smoke.sh`（`MONOENGINE_GIT_SMOKE_PUSH=1`）、`bin/tests/integration_git_cli.rs`。

---

## 2. Tag：禁止 Git 客户端；仅 Web / API

### 规则

1. MonoRepo **禁止** 通过 Git 客户端创建、更新或删除 tag（含 `git push --tags`、`git push origin refs/tags/<name>`、`git push origin :refs/tags/<name>`）。
2. Tag 的创建 / 查询 / 删除 **只允许** Web 界面，并经由对应 HTTP API 完成。
3. ImportRepo（`import_dir` 下）不受本条约束。

### API 表面（Web 后端）

实现：`src/api/router/tag_router.rs`，服务：`MonoApiService::{create_tag,list_tags,get_tag,delete_tag}`。

| 操作 | HTTP（OpenAPI 登记） |
|---|---|
| 创建 | `POST` … `/tags` |
| 列表 | `GET` … `/tags/list` |
| 查询 | `GET` … `/tags/{name}` |
| 删除 | `DELETE` … `/tags/{name}` |

协议层：MonoRepo receive-pack 对 `RefTypeEnum::Tag` **拒绝**更新（返回可诊断错误），不得静默写入 `refs/tags/*`。

### 测试与 CI 期望

| 场景 | 期望 |
|---|---|
| `git push origin refs/tags/<name>`（MonoRepo） | **非零退出**；远端不出现该 tag |
| `git push origin :refs/tags/<name>`（MonoRepo） | **非零退出** |
| Web/API create/list/delete | 由 API / UI 测试覆盖；**不**用 Git CLI smoke 冒充 tag 成功路径 |

权威自动化：`scripts/git_protocol_smoke.sh` 的 tag **拒绝**用例；勿再把「HTTP push and delete tag」写成成功门。

---

## 3. 仓库初始化

### 何时初始化

服务启动时 `Context` → `MonoService::init_monorepo`：

- 若根路径 `/` 已存在 `main` ref → **跳过**（幂等）。
- 否则在同一事务中写入初始 commit、`main` ref、树与 blob 元数据，对象内容进入对象存储。

### 配置输入（`[monorepo]`）

| 字段 | 作用 |
|---|---|
| `import_dir` | ImportRepo 根（默认 `/third-party`）；其下多分支/客户端 tag 合法 |
| `admin` | 初始化时写入 Cedar 实体的系统管理员列表 |
| `root_dirs` | 根树下一层目录名列表（每个目录带独立 `.gitkeep`） |
| `rename.*` | diff 重命名检测参数（非初始化布局字段） |

样例见 `config/config.toml`；生成模板见 `src/config/template.rs`。校验：`monorepo.import_dir` / `root_dirs` / `admin` 非空（`src/config/validate.rs`）。上述字段变更通常要求进程重启（`reload` 的 `restart_required_fields`）。

### 初始化产物（语义）

1. 唯一公开分支 tip：`refs/heads/main`。
2. 根树包含：
   - `root_dirs` 中每个名字对应的一级目录（内含占位 `.gitkeep`）；
   - 根级 `.mega_cedar.json`（由 `admin` 生成实体）；
   - Cedar 策略目录与根级 Buck 相关占位（见 `MegaModelConverter::init` / `init_trees`）；
   - 若 `root_dirs` 含 `toolchains`，该目录额外注入初始化用 BUCK 文件。
3. 不创建任何 `refs/tags/*`；不创建第二个 `refs/heads/*`。

---

## 4. 目录结构

### 逻辑布局（默认 `root_dirs`）

默认配置（`config/config.toml` / `MonoConfig::default`）下，初始化后的**逻辑**根大致为：

```text
/
├── .mega_cedar.json          # 策略实体（admin 写入）
├── .mega_cedar/…             # 策略目录（初始化注入）
├── BUCK 相关根占位           # 见 converter 注入
├── third-party/              # 通常与 import_dir 对齐；其下走 ImportRepo
│   └── .gitkeep
├── project/
│   └── .gitkeep
├── doc/
│   └── .gitkeep
├── release/
│   └── .gitkeep
├── model/                    # 以现场 config.root_dirs 为准
│   └── .gitkeep
└── toolchains/
    ├── .gitkeep
    └── BUCK                  # 初始化时注入（若目录存在）
```

实际一级目录 **以运行配置的 `root_dirs` 为准**；改配置后只影响**尚未初始化**的新库，已初始化库不会自动重建树。

### ImportRepo vs MonoRepo 路径

| 路径 | 类型 | 分支 | Tag（Git 客户端） |
|---|---|---|---|
| `import_dir` 及其子路径 | ImportRepo | 可多分支 | 允许（按 Import 语义） |
| 其余路径（含 `/`） | MonoRepo | 仅公开 `main` + `refs/cl/*` | **禁止**；走 Web/API |

路径判定与 smart HTTP 入口见 `protocol.md` / `GitProtocolPath`。

### 物理存储

逻辑树存在 Postgres（commit/tree/blob 元数据 + refs）与对象存储（blob 内容）；**不是** 工作区磁盘上的普通 Git bare 目录。运维路径由 `base_dir`、数据库与 orbit 对象存储配置决定，见 `docs/refactoring/config.md` / `orbit.md`。

---

## 5. Push 语义状态与对象 `commit_id` 归属

### PushChain：base/tip 的唯一事实源

- MonoRepo receive-pack 的 push 语义状态是 `PushChain { base, tip, ordered_commits }`（`src/ceres/pack/push_chain.rs`），解包后由 `RefCommand.old_id/new_id` 加 tip commit 的 parent 链构建；**不得**从 pack 对象到达顺序推断 base/tip/归属。
- `base` = `RefCommand.old_id`（新分支 push 即 `old_id` 为零值时取 tip 的第一个 parent），即 CL 的 `from_hash`；`tip` = `new_id` 对应的 commit，即 CL 的 `to_hash`。
- CL ref（`refs/cl/<link>`）的 `ref_commit_hash` 与 `ref_tree_hash` 同源自 tip commit（`new_id` 及其 tree）；文件路径索引按 tip 的 tree 遍历。

### 空 pack / 已知 `new_id` = 幂等 no-op（ADR-MC-05）

- pack 中无新 commit 且 `new_id` 指向服务端已知 commit 时，push 成功（report-status ok），CL ref 与 CL 均不变动，report-status 附 `remote:` 提示信息；不为此建 CL。

### 对象 `commit_id` 归属语义（ADR-MC-06）

- `mega_tree` / `mega_blob` 的 `commit_id` 归属 = **链 tip**，语义为「该对象在本路径最后一次被某次 push 触及时的链 tip」。
- 该字段存在**存活读取点**：文件浏览的「最后提交」列（`item_to_commit_map` → `tree_ops.rs` → `preview_router.rs`）。在单 commit/push 强约束下数值恰好正确（tip = 本次唯一 commit）；MC-06 放开多 commit push 后归属 = 链 tip 成为**近似语义**（已记录的非精确值，精确化属 DEFER-MC-03）。
- 任何新增读取该字段的功能必须先重审 ADR-MC-06（`docs/plan/plan-20260827.md`）。

### GPG 签名策略：逐 commit 链式验证（ADR-MC-03/08/09）

- CL 的 GPG 检查（`gpg_signature_checker`）对 `(from_hash, to_hash]` 链上**每个 commit** 逐一验签，不只验 tip；验签载荷 = 由持久化列确定性重建的完整 commit 字节（tree + parents + author + committer + message，剔除 `gpgsig` 块），与 `git verify-commit` 等价；篡改 tree/parent/author/committer 任一字段即验签失败。
- 验签身份：从签名包解析 issuer fingerprint（hashed subpacket 须恰好解析出唯一值，缺失/多值/与 unhashed 冲突一律 fail-closed），反查 `gpg_key.fingerprint`（unique）定位 key 与所属用户；**不认 committer email，不回退 CL owner**。行为变化两处：**非 CL owner 的已注册用户签名可通过**；**未注册 key 的签名 fail-closed**。
- 链长上限 `MAX_CL_CHAIN_COMMITS = 250`（CL 累计范围口径，ADR-MC-07），超限拒绝并提示拆分链；`from_hash` 不在链上（断链）fail-closed。
- merge 门控（最小接通）：存在 `check_type_code=GpgSignature` 且 `status=FAILED` 检查结果的 CL 不得 merge；无任何检查行的 CL 不被阻断。

---

## 6. 交叉引用

| 主题 | 文档 / 代码 |
|---|---|
| 协议与 smoke 矩阵 | [`refactoring/protocol.md`](./refactoring/protocol.md)、`scripts/git_protocol_smoke.sh` |
| HTTP Git CLI IT | `bin/tests/integration_git_cli.rs` |
| Tag REST | `src/api/router/tag_router.rs` |
| Push 状态模型 / `commit_id` 归属 | `src/ceres/pack/push_chain.rs`、`docs/plan/plan-20260827.md`（ADR-MC-05/06） |
| GPG 链式验签与 merge 门控 | `src/ceres/merge_checker/gpg_signature_checker.rs`、`docs/plan/plan-20260827.md`（ADR-MC-03/07/08/09） |
| 初始化实现 | `src/jupiter/service/mono_service.rs::init_monorepo`、`src/jupiter/utils/converter.rs` |
| 配置样例 | `config/config.toml` `[monorepo]` |
| 本地开发入口 | [`development.md`](./development.md) |

修订本规则时：同步更新本文、`protocol.md` 场景表、smoke/IT 期望，以及（若行为变更）MonoRepo receive-pack 实现。
