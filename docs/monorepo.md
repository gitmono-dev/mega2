# Monorepo 产品规则

本文是 mega2 **Monorepo（非 `import_dir` 下的 ImportRepo）** 的集中规则事实源。协议实现、CI smoke、Web/API 与测试矩阵凡涉及分支、标签或仓库初始化，以本文为准；细节实现锚点见 [`refactoring/protocol.md`](./refactoring/protocol.md) 与 `config/config.toml` 的 `[monorepo]`。

> **范围**：默认路径下的 Monorepo（根路径 `/` 及非 `import_dir` 子树）。  
> **例外**：`[monorepo].import_dir`（默认 `/third-party`）下的 **ImportRepo** 仍可按普通 Git 多分支 / 客户端 tag 语义工作；本文规则不覆盖 ImportRepo。

---

## 1. 公开分支：仅 `main`

### 规则

1. Monorepo **只有一个公开分支**：`refs/heads/main`（简称 `main`）。
2. 客户端对 mega2 的 Git 推送 **不得** 在远端创建第二个公开分支（例如 `refs/heads/feature` 不得作为持久 heads 出现）。
3. `git ls-remote` / upload-pack 广告中，heads 侧对用户可见的稳定公开 tip 是 `main`；变更评审用的 tip 落在 `refs/cl/*`，不是公开分支。

### 与 Change List（CL）的关系

- 客户端常见写法 `git push origin HEAD:refs/heads/<name>`（`<name> ≠` 持久公开分支）在 Monorepo 上会进入 **CL 管线**：服务端落地为 `refs/cl/<id>`，**不会**把 `<name>` 登记为第二个 `refs/heads/*` 公开分支，也 **不会** 直接改写 `main`。
- 合并进 `main`、关闭/更新 CL 等产品动作走 Web / 内部 API，而不是「再 push 一个公开分支」。
- 因此：smoke / 集成测试里的「branch push」验收的是 **CL 创建与定向 fetch**，不是多分支托管。
- **`push_policy=trunk` 例外**：CL 管线不参与推送落地；唯一公开分支仍是 `main`，推送 `refs/heads/main` 经队列写入 `main`，其它 heads 在 B0 被拒绝。见第 8–11 节与 [`deploy-trunk.md`](./deploy-trunk.md)。

### 测试与 CI 期望

| 场景 | 期望 |
|---|---|
| 推送非 `main` 的 heads 命令 | 成功时只新增 `refs/cl/*`；`refs/heads/<smoke-name>` **不** 出现在 `ls-remote` |
| 推送后 `ls-remote refs/heads/*` | 仍以 `main` 为公开 heads（无新增公开分支名） |
| 对新建 `refs/cl/*` 的 fetch/checkout/pull | 工作树与 push 前本地树一致（见 `protocol.md` / `integration_git_cli`） |

权威自动化：`scripts/git_protocol_smoke.sh`（`MEGA2_GIT_SMOKE_PUSH=1`）、`bin/tests/integration_git_cli.rs`。

---

## 2. Tag：禁止 Git 客户端；仅 Web / API

### 规则

1. Monorepo **禁止** 通过 Git 客户端创建、更新或删除 tag（含 `git push --tags`、`git push origin refs/tags/<name>`、`git push origin :refs/tags/<name>`）。
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

协议层：Monorepo receive-pack 对 `RefTypeEnum::Tag` **拒绝**更新（返回可诊断错误），不得静默写入 `refs/tags/*`。

### 测试与 CI 期望

| 场景 | 期望 |
|---|---|
| `git push origin refs/tags/<name>`（Monorepo） | **非零退出**；远端不出现该 tag |
| `git push origin :refs/tags/<name>`（Monorepo） | **非零退出** |
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
| `object_format` | 空 Monorepo 初始 object graph 的 ID 格式：`sha1`（默认）或 `sha256`；不会转换既有仓库 |
| `rename.*` | diff 重命名检测参数（非初始化布局字段） |

样例见 `config/config.toml`；生成模板见 `src/config/template.rs`。校验：`monorepo.import_dir` / `root_dirs` / `admin` 非空，且 `object_format=blake3` 仍 fail-closed（`src/config/validate.rs`；启用需单独计划的 repository hash context）。上述字段变更通常要求进程重启（`reload` 的 `restart_required_fields`）。

`object_format` 只控制 `init_monorepo` 的同步初始物件建构。`sha256` 会产生 64 位 initial commit/tree/blob ID；当前 Ceres v1/v2、zero ID 和 pack/runtime context 仍是 SHA-1-only，因此它不是可对外 clone/fetch/push 的完整 SHA-256 仓库格式。`blake3`（正确拼写；不是 `black3`）保留为配置接口，待显式 repository hash context 与下游协议接入完成后再启用（与 `git-internal` 版本解耦，单独计划）。

只能在受控的 `mega2 --config <path> service init --yes` invocation 使用
`sha256`，不能以这个设置启动一般 Git 服务；该命令完成初始图建构后即退出，不启动
Git listener。它要求已有配置，并只装配初始化所需的 DB、必要时 readonly Vault、
object storage 与 MonoService；不会启动 Redis、notification、reload watcher
或任何 Git listener。

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

### ImportRepo vs Monorepo 路径

| 路径 | 类型 | 分支 | Tag（Git 客户端） |
|---|---|---|---|
| `import_dir` 及其子路径 | ImportRepo | 可多分支 | 允许（按 Import 语义） |
| 其余路径（含 `/`） | Monorepo | 仅公开 `main` + `refs/cl/*` | **禁止**；走 Web/API |

路径判定与 smart HTTP 入口见 `protocol.md` / `GitProtocolPath`。

### 物理存储

逻辑树存在 Postgres（commit/tree/blob 元数据 + refs）与对象存储（blob 内容）；**不是** 工作区磁盘上的普通 Git bare 目录。运维路径由 `base_dir`、数据库与 orbit 对象存储配置决定，见 `docs/refactoring/config.md` / `orbit.md`。

---

## 5. Push 语义状态与对象 `commit_id` 归属

### PushChain：base/tip 的唯一事实源

- Monorepo receive-pack 的 push 语义状态是 `PushChain { base, tip, ordered_commits }`（`src/ceres/pack/push_chain.rs`），解包后由 `RefCommand.old_id/new_id` 加 tip commit 的 parent 链构建；**不得**从 pack 对象到达顺序推断 base/tip/归属。
- `base` = `RefCommand.old_id`；新分支 push（`old_id` 为零值）时沿 tip 首父链反走，取第一个**非本 push 新引入**的 commit（fork point；unpack 时已存在于服务端的祖先——即便 pack 冗余携带——不构成链的一部分，Codex R1 P1-1）。`base` 即 CL 的 `from_hash`；`tip` = `new_id` 对应的 commit，即 CL 的 `to_hash`。
- CL ref（`refs/cl/<link>`）的 `ref_commit_hash` 与 `ref_tree_hash` 同源自 tip commit（`new_id` 及其 tree）；文件路径索引按 tip 的 tree 遍历，链上任意 commit 引入/重命名的文件都在覆盖范围内。
- finalize 先过接收闸门再写任何 ref/CL：多 branch 命令拒绝（ADR-MC-04）+ 链校验（MC-03 校验器，增量段复用 resolve 反走的结果不重复读库，历史段仅计数）+ 链外 junk commit 拒绝（pack 携带的每个 commit 必须落在 tip 的首父路径上；冗余携带的已知祖先在路径上故不误伤）。所有拒绝规则只基于 pack **内容**（presence），与 unpack 时的瞬时「新引入」状态无关——被拒 push 原样重试必被同样拒绝（粘性，Codex R2 P1-1）；失败时本 push 的全部 branch 命令以 `ng <ref> <原因>` 回报，对象落库保持 insert-only 语义（被拒 push 的新行是不可达垃圾，不改动既有行）。
- commit 绑定（`commit_auths`）只在 finalize 成功之后写入，且仅覆盖已接受链中**本 push 新引入**的 commit——被拒 push 不得 upsert 绑定，无新引入的 push 不重绑存量（Codex R1 P1-2 / R2 P1-2）。

### 多 commit 链式 push（MC-06 起放开）

- 单次 push 允许携带 **2..=250 个 commit 的线性链**；每次链 push 恰好创建/更新**一个** CL，`from_hash` = 链 base 基线、`to_hash` = 链 tip（ADR-MC-02）。
- 链必须是线性的：含 merge commit（双父）、parent 断链、环或 tip 与 ref 更新目标不符的 push 一律 fail-closed 拒绝，报错引导 rebase 成线性历史后重推（`PushChain::validate`，MC-03 落地、MC-06 接通到 receive-pack 主路径）。
- 链长上限的单一口径 = **CL 累计范围** `(from_hash → to_hash)` ≤ `MAX_CL_CHAIN_COMMITS = 250`（ADR-MC-07）：对既有 open CL 的更新 push 除增量校验外还校验 `cl.from_hash → 新 tip` 的累计长度，超限在 push 阶段拒绝，文案引导「先合并当前 CL 或开新 CL / squash 后重推」。
- 单次 receive-pack 携带多于一条非删除 branch 命令时**整体拒绝**，文案引导分次 push；删除命令不受影响（ADR-MC-04）。
- trunk 侧不变（ADR-MC-01）：CL merge 仍由服务端合成**单父新 commit** 推进 `main`，不做向用户链的 fast-forward。

### 空 pack / 已知 `new_id` = 幂等 no-op（ADR-MC-05）

- pack 中无新 commit 且 `new_id` 指向服务端已知 commit 时，push 成功（report-status ok），CL ref 与 CL 均不变动，report-status 附 `remote:` 提示信息；不为此建 CL。

### 对象 `commit_id` 归属语义（ADR-MC-06）

- `mega_tree` / `mega_blob` 的 `commit_id` 归属 = **链 tip**，语义为「该对象在本路径最后一次被某次 push 触及时的链 tip」。
- 该字段存在**存活读取点**：文件浏览的「最后提交」列（`item_to_commit_map` → `tree_ops.rs` → `preview_router.rs`）。单 commit/push 强约束下数值恰好正确（tip = 本次唯一 commit）；多 commit push 放开后（MC-06 已生效）归属 = 链 tip，为**近似语义**（已记录的非精确值，精确化属 DEFER-MC-03）。
- 任何新增读取该字段的功能必须先重审 ADR-MC-06（`docs/plan/plan-20260827.md`）。

### GPG 签名策略：逐 commit 链式验证（ADR-MC-03/08/09）

- CL 的 GPG 检查（`gpg_signature_checker`）对 `(from_hash, to_hash]` 链上**每个 commit** 逐一验签，不只验 tip；验签载荷 = 由持久化列确定性重建的完整 commit 字节（tree + parents + author + committer + message，剔除 `gpgsig` 块），与 `git verify-commit` 等价；篡改 tree/parent/author/committer 任一字段即验签失败。
- 验签身份：从签名包解析 issuer fingerprint（hashed subpacket 须恰好解析出唯一值，缺失/多值/与 unhashed 冲突一律 fail-closed），反查 `gpg_key.fingerprint`（unique）定位 key 与所属用户；**不认 committer email，不回退 CL owner**。行为变化两处：**非 CL owner 的已注册用户签名可通过**；**未注册 key 的签名 fail-closed**。
- 链长上限 `MAX_CL_CHAIN_COMMITS = 250`（CL 累计范围口径，ADR-MC-07），超限拒绝并提示拆分链；`from_hash` 不在链上（断链）fail-closed。
- merge 门控（最小接通）：存在 `check_type_code=GpgSignature` 且 `status=FAILED` 检查结果的 CL 不得 merge；无任何检查行的 CL 不被阻断。

### 服务端合成 commit 签名（MC-09，REL-MC-02）

- **范围**：只有会进入 CL 链（`from_hash→to_hash` 范围、会被逐 commit 验签）的服务端合成 commit 才签名，共两个来源——`update_branch` rebase 链（`process_ref_updates_cl_only` 内部签名）与 buck 上传链（`complete_buck_upload` 在 `build_commit` 返回后签名重算）。merge 到 trunk 的 commit 与仓建根 commit 不属于任何 open CL 链范围，**明确不签名**（永久非目标）。
- **身份与载荷**：合成 commit 的 author/committer = 固定服务端身份常量（`SERVER_SIGNING_NAME` / `SERVER_SIGNING_EMAIL`，`src/contract/vault/server_signing.rs`，单一事实源，MC-11 身份判定复用）；签名载荷 = ADR-MC-09 规范化完整 commit 字节（tree + parents + author + committer + message），`gpgsig` 以 header 形式嵌入 message（`gpgsig ` 起行、续行单空格前缀、后随空行 + message），与验签侧 header 区锚定提取兼容；对存量无签名合成 commit 的 CL 为行为收紧。
- **原子契约**：「签名 → 重算 commit ID → 统一回填派生引用」不可拆分——`mega_commit.commit_id`、`mega_tree.commit_id`（逐项回填，构造层定型后不联动）、CL ref `ref_commit_hash`、`mega_cl.to_hash` 四值必须同为最终已签名 hash；禁止调用方后处理签名。
- **Fail-closed**：合成点经 `Storage::vault()` 获取 vault 句柄，`None` 即拒绝产出（server signing unavailable）；签名预检（vault 读取 + 密钥解析 + 签名计算）在任何 ref/commit/tree/CL 持久化之前完成，失败零副作用。
- **密钥管理（版本化，私钥不出 Vault）**：`server-signing/keys/<key-id>` 存各代密钥（`key-id` = fingerprint，与 `gpg_key.fingerprint` 同格式），`server-signing/active` 为签名指针，`server-signing/index` 为追加式全量索引；签名只用 active；验签侧（MC-11）经 `list_server_signing_public_keys` 遍历未撤销历史公钥。**轮换只增不删**：历史密钥永不删除/重写，数据不变量 = 既有签名 commit 始终可由历史公钥验证。
- **首次初始化**：经既有 RedLock（`src/jupiter/redis/lock.rs`）跨副本互斥，锁内重读 `active` 指针确认——并发首启时唯一胜者生成密钥，其余副本重读到同一密钥。
- **手动轮换指引**（自动化尚未排期）：当前**没有**操作面板、CLI 子命令或 admin 路由暴露轮换/停用入口，也没有写路径会把 `revoked` 置位；轮换与停用需由运维以自定义脚本直接调用 Rust API 执行——轮换调 `VaultCore::rotate_server_signing_key`（`src/contract/vault/server_signing.rs`，同一 RedLock 互斥）生成新代、追加索引并移动 `active` 指针，旧代保留，新合成 commit 即用新代签名；停用某代 = 经 `VaultCoreInterface::write_secret` 将该代 `server-signing/keys/<key-id>` 的 `revoked` 置 `true`（只改标志、不删密钥体），验签侧 `list_server_signing_public_keys` 随之跳过该代。**顺序规则**：停用当前 active 代之前必须先轮换出新一代（active 指向可用代）——签名端拒绝加载 revoked 的 active 代（fail-closed），直接停用当前 active 代会让两个合成点停摆。

---

## 6. 墓碑与升级回填

路径 `main` ref 被删除后记入 `mega_ref_tombstones`。再次 advertise / 懒物化时：

- 路径仍在当前根树中 → 以墓碑 `last_commit_hash` 为 parent 续接，成功后删除墓碑行。
- 路径不在根树（目录尚未重建）→ **跳过物化**，不凭空造行。
- 对仍有墓碑的路径，`old_id = ZERO_ID` 的创建推送在 B0 被拒绝：先重建父目录 → advertise 续接物化 → fetch 对齐 → 再推送。

**升级前的历史删除**（升级前 `remove_none_cl_refs` 不留痕）默认 **fail-closed**：迁移只建空表，不猜测「曾物化后被删」的路径。旧客户端可能遭遇一次 unrelated history。运维可在升级前按删除清单调用 `backfill_tombstones_best_effort`（不保证完备）。

---

## 7. 交叉引用

| 主题 | 文档 / 代码 |
|---|---|
| 协议与 smoke 矩阵 | [`refactoring/protocol.md`](./refactoring/protocol.md)、`scripts/git_protocol_smoke.sh` |
| HTTP Git CLI IT | `bin/tests/integration_git_cli.rs` |
| Tag REST | `src/api/router/tag_router.rs` |
| Push 状态模型 / `commit_id` 归属 | `src/ceres/pack/push_chain.rs`、`docs/plan/plan-20260827.md`（ADR-MC-05/06） |
| GPG 链式验签与 merge 门控 | `src/ceres/merge_checker/gpg_signature_checker.rs`、`docs/plan/plan-20260827.md`（ADR-MC-03/07/08/09） |
| 服务端合成 commit 签名与密钥轮换 | `src/contract/vault/server_signing.rs`、`docs/plan/plan-20260827.md`（MC-09，ADR-MC-09） |
| 初始化实现 | `src/jupiter/service/mono_service.rs::init_monorepo`、`src/jupiter/utils/converter.rs` |
| 配置样例 | `config/config.toml` `[monorepo]` |
| Trunk 直推设计 | [`refactoring/trunk-push.md`](./refactoring/trunk-push.md) |
| Trunk / storage-only 部署 | [`deploy-trunk.md`](./deploy-trunk.md) |
| 本地开发入口 | [`development.md`](./development.md) |

修订本规则时：同步更新本文、`protocol.md` 场景表、smoke/IT 期望，以及（若行为变更）Monorepo receive-pack 实现。

---

## 8. 部署形态：`review`（默认）与 `trunk`

`[monorepo].push_policy` 是部署形态开关（重启生效；缺省 **`review`**）。

| 形态 | 推送落地 | 公开分支 | CL / Issue / reviewer HTTP | LFS |
|---|---|---|---|---|
| `review`（默认） | 非删除分支更新进入 CL 管线，落地 `refs/cl/*`，不直接改 `main` | 仍仅 `main`；`refs/cl/*` 可 advertise | 注册 | 可用（经 `UserStorage`） |
| `trunk` | 子路径 receive-pack 入 `MonoWriteQueue`（`kind=push`），B3 写入 `main` | 仅 `main`；存量已关闭 CL ref **保留为档案**但不 advertise | **不注册**（OpenAPI 如实为空） | 可用（`push_auth`） |

`review` 形态下本文第 1–5 节一字不改。`trunk` 形态下第 1 节「唯一公开分支 `main`」、第 2 节「Git 客户端禁 tag」、`import_dir` 例外继续生效；CL 管线不参与推送落地。推送 `refs/heads/dev` 在 trunk 被 B0 拒绝。

Trunk **强制**显式 `[git].push_auth`（`token` 或 `none`），且与 `cedar.enforcement != "off"` 互斥。配置与启动校验见 [`deploy-trunk.md`](./deploy-trunk.md)。

## 9. 按 N 分流的推送语义与客户端对齐（ADR-TP-12 / ADR-TP-18）

仅 **`push_policy=trunk`**。`N` = 客户端推送链在被推路径 `P` 上的首父链长度（`new_id → old_id`）。

| N | `main@P` | 对象保真 | 客户端 |
|---|---|---|---|
| 1（Agent 常态） | 等于客户端 `cmd.new_id`，**不合成** | 作者 / 时间 / message / 签名随对象保留 | `git fetch && git reset --hard origin/main` 为 no-op |
| > 1 | 一个 squash commit：`tree` = 客户端 tip 的 tree，`parent` = 推送前 tip（创建且无旧 tip 时 parentless） | 不承诺逐字段对象保真；内容保真见 I2 | 成功 sideband 给出落地 id；随后必须 `git fetch && git reset --hard origin/main`。未对齐再推会被 non-fast-forward 拒绝，拒绝信息含同一对齐命令 |

根路径 `/` 的 push 不是该形态的假设（会塌缩根 CAS 与 `P` 落地）。净零推送（被推路径 tree 未变）不推进祖先与 `/`（ADR-TP-16）。

## 10. 写入不变式（I1–I6，含 I2a）

产品层声明；证明与闸门细节以 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md) 附录 A 为准。

- **I1 历史只增不改**：已物化路径的旧 tip 必须仍是新 tip 的祖先。成立时点自阶段 2（墓碑续接）起；阶段 1 的删除式后代处理不得部署生产。
- **I2 内容保真**：`main@P` tip 的 tree 等于客户端 tip 的 tree。N = 1 时更强：tip 等于客户端 commit id。
- **I2a 步长一致**：一次**有树变更**的推送后，被推路径、受影响祖先与 `/` 各恰好前进一个 commit。净零推送时未受影响的层前进零个。
- **I3 视图一致（强一致）**：每个已物化 `main@P.ref_tree_hash` 等于从当前根树解析的子树。路径 ref 是根树的视图，不分层为最终一致。前提是全部根写入者遵守队列锁纪律（I5）。
- **I4 provenance 完整**：N > 1 时被推路径 squash 完整枚举被合并 commit，不截断；创建情形省略 `Mono-Squash-Range`，由 message 与 tip 锚定。
- **I5 队列完备性**：根树写入经 `MonoWriteQueue`，或被物化短锁（ADR-TP-20）覆盖，或在接流前 bootstrap。根 ref CAS 是 tripwire，不是完备性证明。
- **I6 时间线全序**：任意时刻至多一个 B 段；`/` 上 roll-up 的先后与 `push_queue.id` **保序**（允许空洞，例如净零占用 id 却不产生根 commit）。

## 11. 文件路径索引最终一致性（`blob_paths`）

浏览用路径索引是 **`(blob_id, path, indexed_push_id)` 出现对**（`blob_paths`），在 B3 提交之后的 C 段重建，**不在**写入判定链上。

- 索引相对根树 / 路径 ref **最终一致**：推送刚提交后，Web 路径查询可能短暂落后。
- 行级 `indexed_push_id` CAS 防止滞后任务覆盖更新的嵌套路径。
- **不承诺「永不复活」**：旧任务仍可能把已删除的 `(blob_id, path)` 重插；补偿任务周期重扫会再次清除。
- 形态切换时执行 `RESET INDEX WATERMARK`（`indexed_push_id = NULL`），见 [`deploy-trunk.md`](./deploy-trunk.md)。
