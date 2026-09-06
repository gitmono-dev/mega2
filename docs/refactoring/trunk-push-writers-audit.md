# Trunk-push 写入者审计清单（TP-23 / 阶段 1.1）

**审计日:** 2026-09-06  
**锚点刷新:** 对照当时 `src/` 现场行号（GC-01）  
**范围:** `refs/heads/main`（含根 `/` 与路径物化）与根树推进写入者；CL/tag 行登记为范围外。  
**三分法:** `queue-serial`（须经 MonoWriteQueue）/ `adr-tp-20`（队列外、ADR-TP-20 覆盖）/ `bootstrap-lock`（接流前专属锁）/ `out-of-scope` / `test-fixture` / `storage-primitive`（被上层写入者调用的原语，非独立写入者）/ `rg-hit`（命中但非独立写入者：方法签名或注释）

机器核对命令（与计划 TP-23 Verification 对齐）：

```bash
rg -n "save_refs|update_ref|mega_head_hash_with_txn|batch_update" src/ --stats
```

本清单要求：上式**每一个**命中（含定义、调用、注释）要么是下方条目的证据锚点，要么登记为 `test-fixture` / `storage-primitive` / `out-of-scope` / `rg-hit`。另对 `mega_refs` 直接写原语做超集核对（见 §G），补登记 `rg` 模式漏检的 CL 写入者。

---

## A. 硬约束 2 · 改写根树（`queue-serial`）

| ID | Writer | Classification | Evidence | Notes |
|---|---|---|---|---|
| W-ROOT-01 | CL merge → `merge_cl_unchecked` → `apply_update_result` → `batch_update_by_path_concurrent` | `queue-serial` | `src/ceres/api_service/mono_api_service.rs:2582`（`merge_cl_unchecked`）、`:2614`（调用 `apply_update_result`）、`:2651`（`apply_update_result`）、`:2694`（`batch_update_by_path_concurrent`） | 生产调用方：`merge_cl` `:2126`、merge-queue processor `:4603`。阶段 1（TP-07）须全部入队。 |
| W-ROOT-02 | ImportRepo attach（含分支命令）→ `attach_to_monorepo_parent` → `attach_to_monorepo_parent_in_txn` | `queue-serial` | `src/ceres/pack/import_repo.rs:450`（`attach_to_monorepo_parent`）、`:577`（`attach_to_monorepo_parent_in_txn`）；原语 `src/jupiter/storage/mono_storage.rs:453` | 触发：`import_repo.rs:109`。双条件 CAS + 重试循环（阶段 1 吸收后须删除重试）。 |
| W-ROOT-03 | trunk 形态推送落地（阶段 4 引入） | `queue-serial`（planned） | 设计登记：`docs/refactoring/trunk-push.md:108`（硬约束 2「trunk 形态下的推送落地」）；现状 review 推送只写 `refs/cl/*`（`monorepo.rs:913-971`） | **现状无生产代码路径**。TP-12/TP-17 落地后把证据改为实现 `file:line`。 |

附属（merge 后清理；**现状非同事务**——见 W-ROOT-01a）：

| ID | Writer | Classification | Evidence | Notes |
|---|---|---|---|---|
| W-ROOT-01a | merge 后代清理 `remove_none_cl_refs` | `queue-serial`（附属删除；阶段 1 须事务化变体，阶段 2 由 `advance_descendant_refs` 取代） | 调用 `mono_api_service.rs:2619`（在 `apply_update_result` `:2614` **之后**）；原语 `mono_storage.rs:78-84`（自建 `get_connection()`，`Path.starts_with` ∧ `Path != path` ∧ `IsCl=false`） | **非同事务**：根/`main` 推进经 `batch_update_by_path_concurrent` 已提交后，清理另开连接执行——崩溃或清理失败可留下「根已前进、路径非 CL 行（含 tags）仍陈旧」。删除范围是全部非 CL 后代（含路径 tags），不限 `main`。违反 I1；不得当作已原子化的队列工作。tag 删除面另见 W-OOS-09a。 |

---

## B. 硬约束 2 · 写路径 `main` ref（`adr-tp-20`）

| ID | Writer | Classification | Evidence | Notes |
|---|---|---|---|---|
| W-PATH-01 | advertise/clone/ls-remote 惰性物化 `Monorepo::refs_with_head_hash` | `adr-tp-20`（`main`）+ 附带 tag 副作用见 W-OOS-09 | `src/ceres/pack/monorepo.rs:121`（入口）、`:140`（`get_all_refs("/", true)`）、`:185-194`（按 `root_ref.ref_name` 写路径行，含 `mega_head_hash_with_txn`）；协议触达 `smart.rs:81`、`v2.rs:70` | 触发条件：路径尚无 `main` 行。`get_all_refs("/", true)` 只滤 `is_cl=false`，故**根上全部非 CL 行（含 `main` 与 tags）**都会被复制到路径。I3 相关的是路径 `main`；tag 复制登记为 W-OOS-09。 |
| W-PATH-02 | code_edit 惰性物化 `create_repo_commit` | `adr-tp-20`（`main`）+ 附带 tag 副作用见 W-OOS-09 | `src/ceres/code_edit/utils.rs:362`（入口）、`:377`（`get_all_refs("/", true)`）、`:419-428`（同形复制 + `mega_head_hash_with_txn`）；调用 `utils.rs:66` | 同上；ADR-TP-20 三层覆盖（TP-10/11）针对路径 `main`；tag 副作用见 W-OOS-09。 |

---

## C. 硬约束 2 · bootstrap（`bootstrap-lock`）

| ID | Writer | Classification | Evidence | Notes |
|---|---|---|---|---|
| W-BOOT-01 | `initialize_monorepo` 首次写入根 `main` | `bootstrap-lock` | `src/jupiter/service/mono_service.rs:103`（`initialize_monorepo`）、`:105`（`acquire_monorepo_initialization_lock`）、`:126`（`converter.refs.insert`）；根身份构造 `src/jupiter/utils/converter.rs:727-746`（`path="/"`、`ref_name=MEGA_BRANCH_NAME`、`is_cl=false`） | 两条入口共用同一实现：`init_monorepo` `:85`（常态服务）、`bootstrap_monorepo` `:95`（one-shot `service init`）。 |

### Bootstrap 三证据（TP-23 AC）

1. **接流顺序（分路径，不可混为一谈）:**
   - **常态 HTTP/SSH 服务:** `AppContext::new`（`src/commands/service/mod.rs:51`）→ `new_with_monorepo_initialization`（`src/context/mod.rs:118`）→ `init_monorepo`（`:281`）完成初始化后，才启动协议任务（`src/commands/service/multi.rs:53-60` 绑定 HTTP/SSH）。
   - **one-shot `service init`:** 仅调 `bootstrap_monorepo`（`src/commands/service/init.rs:19-22`）后退出，**不**进入 HTTP/SSH 接流；仍持同一专属锁写根 `main`。
2. **专属锁调用点:** `acquire_monorepo_initialization_lock`（`mono_service.rs:63-70`），事务级 advisory lock SQL，于 `:105` 在写根 ref 前持锁。
3. **不入队理由:** 发生在服务接流前（或独立 init 命令且不接流）、持专属锁、一次性初始化；无并发根写入者可与之交错，故不经 `MonoWriteQueue`（I5 三分法第三类）。

---

## D. 范围外（硬约束 2 审计范围限定）

| ID | Writer | Classification | Evidence | Notes |
|---|---|---|---|---|
| W-OOS-01 | CL ref 行（code_edit） | `out-of-scope` | `src/ceres/code_edit/on_edit.rs:47`（`save_refs` CL） | 非 `main`；CL 管线门控。 |
| W-OOS-02 | CL ref 行（review 推送落地） | `out-of-scope` | `monorepo.rs:913-971`（`apply_cl_mega_ref_for_push_command`：`update_ref`/`save_refs`） | review 形态只写 `refs/cl/*`，不触碰根/`main`。 |
| W-OOS-03 | CL ref 行（rebase / 同步） | `out-of-scope` | `mono_api_service.rs:3392`、`:3446`（`update_ref` CL） | CL tip 跟随，非 `main`。 |
| W-OOS-04 | tag ref 行（创建） | `out-of-scope` | `mono_api_service.rs:1927`、`:1978`（`save_refs` tag，`is_cl=false`） | tag API；不参与 I3。 |
| W-OOS-04a | tag ref 行（删除） | `out-of-scope` | `mono_api_service.rs:1356`、`:1373`（`remove_ref` `refs/tags/*`） | 与 W-OOS-04 同属 tag 面；删除路径。 |
| W-OOS-05 | 纯删除式 ImportRepo attach | `out-of-scope` | `import_repo.rs:450-475`（无非零分支命令时只删 ImportRepo 自身 refs） | 不触碰根树 / `mega_refs` `main`。 |
| W-OOS-06 | ImportRepo / git_db 自身 refs | `out-of-scope` | `git_db_storage.rs:75/114/163`；`ImportRepo::update_refs` 签名 `:333` + 体 `:352`；`:569`（`update_ref_in_txn`）；`import_api_service.rs:467` | `git_*` 表，非 `mega_refs`。签名 `:333` 亦为 `rg` 命中。 |
| W-OOS-07 | review 非-main 分支删除 | `out-of-scope` | `monorepo.rs:916-939`（`remove_ref`，`:937`） | UN-16 护 `main`（`:925`）；删除不经队列。 |
| W-OOS-08 | CL ref 行（Buck 推送路径） | `out-of-scope` | 调用方 `src/jupiter/service/buck_service.rs:928-938`（`save_or_update_cl_ref_in_txn`，`refs/cl/{cl_link}`）；写点 `mono_storage.rs:248-280` | **`rg` 模式漏检**（不经 `save_refs`/`update_ref`）；超集核对补登。永不写 `main`（`is_cl` / `refs/cl/*`）。 |
| W-OOS-09 | 惰性物化附带的路径 tag 行写入 | `out-of-scope`（W-PATH-01/02 副作用） | `monorepo.rs:140-194`、`code_edit/utils.rs:377-428`：对 `get_all_refs("/", true)` 返回的**每个**非 CL 根行（含 tags）按 `root_ref.ref_name` 写入路径 | 非 I3 判定链；硬约束 2 审计主范围仍是 `main`，但 tag 复制是真实生产写入，不得标成「仅 main」。 |
| W-OOS-09a | merge 清理附带的路径 tag 删除 | `out-of-scope`（W-ROOT-01a 副作用） | 同 W-ROOT-01a：`mono_api_service.rs:2619` → `mono_storage.rs:78-84`（另连接、非同事务） | 与 W-ROOT-01a 同一调用；显式登记以免「仅删 main」或「tag 仅由 tag API 管理」误解——惰性物化写入的路径 tags 会在此被删。后续队列工作不得因硬约束 2「tag API 独立」措辞而省略本条目。 |

---

## E. 存储原语（非独立写入者）

| ID | Symbol | Classification | Evidence | Called by |
|---|---|---|---|---|
| P-01 | `MonoStorage::save_refs` | `storage-primitive` | `mono_storage.rs:65` | W-PATH via `mega_head_hash_with_txn`；W-OOS CL/tag；fixtures |
| P-02 | `MonoStorage::update_ref` | `storage-primitive` | `mono_storage.rs:188` | W-OOS CL；fixtures |
| P-03 | `MonoStorage::batch_update_by_path_concurrent` | `storage-primitive` | `mono_storage.rs:287` | W-ROOT-01 |
| P-04 | `MonoStorage::mega_head_hash_with_txn` | `storage-primitive` | `mono_storage.rs:485`（内调 `save_refs` `:491`） | W-PATH-01/02 |
| P-05 | `MonoStorage::attach_to_monorepo_parent_in_txn` | `storage-primitive` | `mono_storage.rs:453` | W-ROOT-02 |
| P-06 | `MonoStorage::remove_none_cl_refs` | `storage-primitive` | `mono_storage.rs:78` | W-ROOT-01a |
| P-07 | `RepoHandler::update_refs`（trait / Monorepo tag 拒绝面） | `storage-primitive` / 协议挂点 | `pack/mod.rs:302`；`monorepo.rs:676`；触发 `smart.rs:450` | Tag 命令拒绝；分支命令分支不可达（事实校准 1） |
| P-08 | `MonoStorage::save_or_update_cl_ref` / `_in_txn` | `storage-primitive` | `mono_storage.rs:215`、`:248-280` | W-OOS-08（Buck） |

---

## F. 测试 fixture（生产外）与其它 `rg` 命中

| ID | Site | Classification | Evidence |
|---|---|---|---|
| T-01 | `admin_ops` seed `save_refs` main | `test-fixture` | `admin_ops.rs:220`（`#[cfg(test)]`） |
| T-02 | `un19_fail_closed` seed `save_refs` main | `test-fixture` | `un19_fail_closed.rs:88`（`#[cfg(test)]`） |
| T-03 | `mono_api_service` tests `setup_main_ref` / CL fixtures | `test-fixture` | `mono_api_service.rs:5777`、`:6337`、`:6523`、`:6667` |
| T-04 | `import_repo` tests 调 `attach_to_monorepo_parent` | `test-fixture` | `import_repo.rs:872`、`:956` |
| T-05 | 测试注释提及 `batch_update_by_path_concurrent`（非调用） | `rg-hit`（comment） | `mono_api_service.rs:5918` |

---

## G. `rg` 交叉核对结论（2026-09-06）

对 `rg -n "save_refs|update_ref|mega_head_hash_with_txn|batch_update" src/` 的**全部**命中分类结果（逐条）：

| 命中 | 登记 |
|---|---|
| `mono_storage.rs:65/188/287/485/491` | P-01..P-04 |
| `git_db_storage.rs:75/114/163` | W-OOS-06 |
| `code_edit/utils.rs:428` | W-PATH-02 |
| `code_edit/on_edit.rs:47` | W-OOS-01 |
| `monorepo.rs:194` | W-PATH-01 |
| `monorepo.rs:676` | P-07 |
| `monorepo.rs:960/969` | W-OOS-02 |
| `pack/mod.rs:302` | P-07 |
| `import_repo.rs:333` | W-OOS-06（签名） |
| `import_repo.rs:352/569` | W-OOS-06 |
| `admin_ops.rs:220` | T-01 |
| `un19_fail_closed.rs:88` | T-02 |
| `mono_api_service.rs:1927/1978` | W-OOS-04 |
| `mono_api_service.rs:2694` | W-ROOT-01 |
| `mono_api_service.rs:3392/3446` | W-OOS-03 |
| `mono_api_service.rs:5777/6337/6523/6667` | T-03 |
| `mono_api_service.rs:5918` | T-05（注释） |
| `import_api_service.rs:467` | W-OOS-06 |
| `smart.rs:450` | P-07 |

**超集补登（`rg` 模式不可见）:**

- W-BOOT-01：`converter.refs.insert`（ActiveModel）
- W-OOS-08 / P-08：`save_or_update_cl_ref_in_txn`（Buck）
- W-OOS-04a / W-OOS-07：`remove_ref` 删除面

- **生产根/`main` 写入者:** W-ROOT-01、W-ROOT-02、W-PATH-01、W-PATH-02、W-BOOT-01 — 均已入表；**无清单外生产 `main`/根树写入者**。
- **planned:** W-ROOT-03（trunk 推送）尚未有代码锚点。
- **范围外生产 CL 写入者:** W-OOS-01..03、W-OOS-08（含先前漏登的 Buck 路径）。

**审计结论:** 硬约束 2 三类（`main`/根树）+ 范围外 CL/tag 完备——含惰性物化与 merge 清理的 **tag 副作用**（W-OOS-09/09a），不得将 W-PATH / W-ROOT-01a 表述为「仅 main」。发现 0 个需升级修订硬约束 2 的清单外生产 `main`/根树写入者。结论已回写 `trunk-push.md` 事实校准节（条目 18）。

---

## H. 与计划/硬约束 2 对齐

| 硬约束 2 类别 | 本清单 ID |
|---|---|
| 改写根树 | W-ROOT-01, W-ROOT-02, W-ROOT-03(planned), W-ROOT-01a |
| 写路径 ref | W-PATH-01, W-PATH-02（`main`；tag 副作用 → W-OOS-09） |
| 队列外 bootstrap | W-BOOT-01 |
| 范围外 CL/tag 等 | W-OOS-01..09a |
