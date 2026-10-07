# Mega2 integration tests

This document describes the active integration-test contract. Compose service
registration and lifecycle rules live in [`test-infra.md`](./test-infra.md).

## Planned Libra evidence integration coverage

[`libra.md`](libra.md) §9 registers planned acceptance scenarios **AC-LB-01…15**:
real Libra ingestion, immutable uploads, interrupted sync/GC, task isolation,
revision invalidation, delegation/access control, derived-data privacy, trusted CI,
multi-process landing, provenance, projection replay, crash/restore, retention/deletion,
and optional dependency-graph/SCM integrations. That table is the scenario source of truth.
These are requirement IDs, **not implemented cargo targets or passing tests**.
Implementation must register actual targets and CI coverage using the migrations and
compose lifecycle below; required live-client/multi-process checks must not pass by skipping.
The landing cases depend on the shared root writer specified in `trunk-push.md`.

## Test stack

`docker/docker-compose.test.yml` provides PostgreSQL, Redis, RustFS, optional
profiled `git-cli`, profile `app` mega2, and profile `web` website-next.
The default data plane does not start an SMTP capture service. Use the
fixed project name:

```bash
docker compose -p mega2-it -f docker/docker-compose.test.yml up -d --wait
```

The embedded `VaultCore` is part of mega2; no external Vault container is
used. Database schemas must be created through the project's migrations.

## Active integration targets

Git 用户场景的完整矩阵（HTTP/SSH/auth/repo-shape、字面 `git pull`、LFS、DEFER）以
[`protocol.md` 的「场景覆盖表」](./protocol.md#场景覆盖表权威plan-20260803--adr-gm-01)
为唯一事实源；本文件只登记 cargo target 索引，不复制矩阵单元格。

| Target | Purpose | Prerequisites |
| --- | --- | --- |
| `integration_vault` | CLI config, Vault bootstrap, redaction, and HTTP smoke | PostgreSQL and Redis |
| `integration_git_cli` | Git HTTP protocol round trips | PostgreSQL, Redis, `--profile git` |
| `integration_git_cli` filter `trunk` / `push_auth` | Trunk 直推 e2e：`integration_git_cli_trunk_n1_identity_three_ff_and_no_cl_refs`、`integration_git_cli_trunk_n_gt1_squash_sideband_and_nff_align`、`integration_git_cli_trunk_requester_token_name`、`integration_git_cli_push_auth_token_rejects_without_token_and_out_of_path` | PostgreSQL, Redis；本机防火墙下 compose `git-cli` 可能不可达，用例走 host git + `127.0.0.1` |
| `integration_git_lfs` | Git HTTP LFS push → fetch → `git lfs pull` round trip（plan-20260803 / GM-05 review/CL）；WH-05 追加 `integration_git_lfs_storage_events_basic_upload`（basic 一次投递日志）与 `integration_git_lfs_storage_events_presigned_gap`（RustFS 直传零事件，DEFER-WH-01 缺口证据） | PostgreSQL, Redis, `--profile git`, git-lfs |
| `integration_git_lfs` filter `trunk` | storage-only LFS（plan-20260909 / LF-03）：`integration_git_lfs_trunk_push_auth_none_round_trip`、`integration_git_lfs_trunk_push_auth_token_round_trip`；直推 `main` 子路径，非 CL。Compose 黑盒对照：`scripts/git_protocol_smoke_storage_only.sh` case `HTTP LFS push and pull (trunk)` + `config/compose.env.storage-only.lfs-innetwork`（plan-20260906 SO-03 / ADR-SO-04） | PostgreSQL, Redis, git-lfs；本机防火墙下 compose `git-cli` 可能不可达时走 host git + `MEGA2_IT_ALLOW_HOST_GIT=1` |
| `integration_storage_events_media` | storage-only FastCDC media finalize 出站进程级门（plan-20260912 / WH-06）：feature-on/off 两个独立 target-dir 二进制（off：路由 404、零事件；on：OpenAPI 列 media 路由、匿名/静态 token 401、DB token 404 业务证明、零事件、SIGINT 清理尾段）；正向事件由 fastcdc lib collector 覆盖 | PostgreSQL, Redis；嵌套 cargo 构建两个 feature 二进制；`--test-threads=1` |
| `integration_git_ssh` | Git SSH cargo-native self-start clone/pull/push（plan-20260803 / GM-06..08） | PostgreSQL, Redis, `--profile git` |
| `integration_git_ssh` filter `trunk_none` / `token_anon` | storage-only SSH 只读（plan-20260908 / SP-01）：`integration_git_ssh_trunk_none_anon_on_clone`、`integration_git_ssh_trunk_none_anon_off_clone_fail`、`integration_git_ssh_trunk_token_anon_on_clone` | PostgreSQL, Redis；`--profile git` 或 host git |
| `integration_git_ssh` filter `token_anon_off` / `review_pubkey` | storage-only SSH password-token（plan-20260908 / SP-02）：`integration_git_ssh_trunk_token_anon_off_clone_fail`、`integration_git_ssh_trunk_token_anon_off_password_clone`、`integration_git_ssh_trunk_token_anon_off_password_fetch`、`integration_git_ssh_trunk_token_anon_off_password_pull`、`integration_git_ssh_trunk_token_push_receive_pack_disabled`、`integration_git_ssh_review_pubkey_anon_off_clone`、`integration_git_ssh_review_pubkey_anon_on_push` | PostgreSQL, Redis；host git + `127.0.0.1`（compose git-cli 无法 hairpin 时） |
| `integration_website_auth` | Better Auth cookie to mega2 session bridge | `--profile app --profile web`, `WEBSITE_IT=1` |
| `integration_authz_audit` | 只读装配零副作用黑盒（UN-30 / UN-43）+ `authz-audit` CLI（UN-29 审计/fsync + UN-37 promote） | PostgreSQL, Redis，且必须 `-- --test-threads=1` |
| `integration_oci` | storage-only OCI `/v2` 进程级黑盒（plan-20260902 / DR-12）：`integration_oci_auth_matrix`、`integration_oci_protocol_walkthrough`、`integration_oci_docker_gated`（daemon 不可用 → SKIP，不 FAIL）；WH-04 追加 `integration_oci_storage_events_publication`（enabled+播种 secret 的 manifest 发布 201 + 一次有界投递日志 + digest 不符 400 + SIGINT 清理尾段） | PostgreSQL, Redis；raw HTTP（reqwest）；docker CLI 仅 `docker_gated` 组可选 |
| `integration_api_write_trunk` | trunk 产品 API 写（plan-20260904 / AW-03；plan-20260917 LB-02..04；plan-20260918 FT-02..07）：token `create-entry` / `edit/save` 前进 tip、无 CL、无凭据 401；`delete_entry_*` 与 `move_entry_*` 覆盖目录与文件删移、双路径 401/403、traversal、ImportRepo 409、缺目录 404、`.gitkeep`、Review CL 复用与 tree 一致性；`tag_*`（8 例，含 `tag_path_isolation_same_name`）覆盖 storage-only tag 路由挂载、trunk 写鉴权、GET list、`(path, name)` 隔离与运行时 OpenAPI。每个 case 各自 boot 一次真实 `service http`。Compose 黑盒对照：`scripts/api_write_smoke_storage_only.sh` 的 `delete-entry-git-visible` / `move-entry-git-visible` / `tags-list-create-delete` / `delete-entry-unauth-401`（LB-05）以及 `delete-file-git-visible` / `move-file-git-visible` / `tags-get-list-create-delete` / `tags-path-get-delete` / `delete-file-unauth-401`（FT-07） | PostgreSQL, Redis；raw HTTP（reqwest）；`push_policy=trunk` + `push_auth=token`；必须 `-- --test-threads=1` |
| `integration_agent_capture` | storage-only Agent Capture `/api/v1/agent-capture` 进程级黑盒（plan-20260911 / AC-13）：`integration_agent_capture_happy_path`、`integration_agent_capture_review_404`、`integration_agent_capture_unauthorized`、`integration_agent_capture_tracing_has_no_raw_sentinel`、`integration_agent_capture_cross_deployment_isolated`、`integration_agent_capture_tombstone_race`；WH-07 追加 `storage_events_batch`（enabled+播种 secret：OpenAPI 仍列 `events:batch`、`push_auth=none` 下无效 ingest token 401、有效 token 200、跨仓库 404、SIGINT 清理尾段；正向投递由 lib collector 覆盖）；WH-08 追加 `storage_events_checkpoint`（OpenAPI 仍列 checkpoints、无效 token 401、新/重复/共享 blob/incomplete 200、跨仓库 404；正向投递由 lib collector 覆盖） | PostgreSQL, Redis；隔离 local object backend；raw HTTP（reqwest）；`--test-threads=1` |
| `integration_storage_events_runtime` | storage-only 出站事件关停接线进程级门（plan-20260912 / WH-13）：默认 disabled 的 `service http` SIGINT 优雅退出并记录 `storage_events_shutdown_complete`、占用端口启动失败仍经清理尾段、`service multi http` SIGINT 退出、`config validate` 无清理日志的 AC7 回归；WH-11 `secret_binding`：enabled+valid secret 经 `config secret set` 种子后启动/SIGINT 退出、enabled+missing 与 wrong-namespace 快速失败且日志无 SecretRef URI（脱敏）、disabled+dangling ref 正常启动 | PostgreSQL, Redis；`--test-threads=1` |
| `integration_storage_events_git` | storage-only 出站 `repo.push` 进程级门（plan-20260912 / WH-03）：enabled+已播种 secret 下真实 host-git trunk push 落地、真实运输记录一次有界投递尝试（类别日志）、SIGINT 经清理尾段退出；review 形态拒绝 enabled；review 分支 push 建 CL 回归。WH-15 追加：每条 emitter 投递/drop 行带播种的 `installation_id`、drop 行不命中投递过滤器、被静态过滤的 seed push 恰好产生一条 `dropped_filter` 行（API 写用例为零条） | PostgreSQL, Redis；host git；`--test-threads=1` |
| `integration_github_sync` | outbound GitHub-sync SSH session（plan-20260916 / GS-07）：`ssh_connect_authenticates` 钉住回环主机钥并对本地 russh 服务端完成 Ed25519 公钥认证 | 回环 only；不连 GitHub；`--test-threads=1` |

## Storage-only compose black-box coverage

These scripts run in the `interop-smoke` service (`--profile interop`) against the
published loopback endpoints of the default `mega2-trunk` stack. They complement,
not replace, the process-level integration targets above. Run them as documented
in [`deploy-trunk.md`](../deploy-trunk.md); each case and its task card are indexed
in [`plan-20261001.md`](../plan/plan-20261001.md).

| Script | Default-stack result | Opt-in coverage |
| --- | --- | --- |
| `scripts/oci_client_smoke_storage_only.sh` | 12 passed, 0 failed (1 skipped) | auth-none case through `scripts/bb_optin_run.sh` |
| `scripts/artifacts_smoke_storage_only.sh` | 14 passed, 0 failed (3 skipped) | auth-none, GC, and local-storage cases through `scripts/bb_optin_run.sh` |
| `scripts/libra_smoke_storage_only.sh` | 24 passed, 0 failed (0 skipped) | None; Libra is the client under test, with Git used only as a read-only observer |

storage-only outbound events（plan-20260912）地址策略、HMAC 运输与有界 emitter 由 lib 测试（`jupiter::service::storage_event_transport` / `storage_event_emitter`）覆盖；WH-15 的 drop 记账与 `installation_id` 记录字段由 `storage_event_emitter::tests::drop_accounting_and_installation_id`（thread-local tracing 捕获，覆盖七类 disposition、per-target 行、`record_invalid_event` 与 disabled/`-` 情形）覆盖，投递行的 `installation_id` 与「drop 行不命中投递过滤器」由 `integration_storage_events_git` 的 `assert_emitter_lines_carry_installation_id` 在真实进程 stdout 上断言；WH-13 的 CLI/service 关停接线与 WH-11 的启动 secret 绑定由进程级 target `integration_storage_events_runtime` 覆盖（见上表）。生产运输没有 HTTP/私网逃逸开关。WH-03 已在 B3 真实 push 提交挂钩（`repo.push`），进程级证据见 `integration_storage_events_git`；WH-07 Agent `events.committed` 的进程级认证/OpenAPI 证据见 `integration_agent_capture` 的 `storage_events_batch`；WH-08 checkpoint 见同 target 的 `storage_events_checkpoint`。

Run the normal project gate with the test environment loaded:

```bash
source .env.test && cargo test --all
```

Integration test sources live under `tests/integration_*.rs` on the `mega2`
package (lib `mega2_core`). Black-box tests drive the real CLI via
`CARGO_BIN_EXE_mega2`.

For the real website-session check:

```bash
docker compose -p mega2-it -f docker/docker-compose.test.yml \
  --profile app --profile web up -d --wait
source .env.test
WEBSITE_IT=1 cargo test -p mega2 --test integration_website_auth -- --test-threads=1
```

When `WEBSITE_IT=1` is set, unavailable mega2 or website-next endpoints
are failures, not passing skips.

## Notification coverage

Mega2 tests cover trigger selection, user preferences, and generic webhook
delivery under `[notification.webhook]`. Product email is not a mega2
outbound path; see the tombstone in [`website-mail.md`](./website-mail.md).

Do not add or retain tests that:

- seed or query `email_jobs`;
- set `[mail]`, `mail.password`, or SMTP endpoint configuration;
- require this repo to deliver mail or start an SMTP capture service;
- treat SMTP capture availability as a mega2 startup gate.

## 授权首建与 shadow→enforce 集成（UN-02）

HTTP 服务启动时在 listener 绑定前完成共享授权快照首建（`ensure_authz_first_build`，ADR-UN-02）：`off` 不构建；`shadow`/`enforce` 下首建失败使 server 启动失败。push 门三态（`check_push_permission`）在 `shadow` 下放行并记录 would-deny，`enforce` 下拒绝无权限 push。SSH 面（UN-03）与 HTTP 面共享同一 `AppContext.entity_store` 实例；独立 `service ssh` 对 `enforcement != off` 拒绝启动（指引 `service multi` 或 `off`）。UN-02 的 5 门 shadow→enforce 判据脚本使用 `MEGA_IT_PG*` 环境变量（登记于 `.env.test.example`，ER-11：凭据只经环境变量）。

## 授权快照传播与主干删除防护集成（UN-16）

`integration_git_cli` 与 `integration_git_ssh` 覆盖快照传播闭合的端到端行为：

- `integration_git_cli_authz_revoke_grant_immediate_effect`：`enforce` 下非 admin push 被拒 → admin 合并授予变更后同一 push 通过 → 合并撤销变更后再次被拒，全程不重启服务。
- `integration_git_cli_rejects_main_branch_delete`：receive-pack 删除主干 ref 被拒绝，错误对 git 客户端可操作。
- `integration_git_ssh_authz_grant_immediate_effect`：ACL 变更经 HTTP 腿的 merge 漏斗合入，SSH 腿在下一次 push 即刻生效——`service multi` 下两腿共享同一实例的直接证据。

授权变更后再次 push 前必须先 `git fetch` 并从新的 `origin/main` 起分支：`Monorepo::check_entry` 只接受每次 push 携带单个 commit，陈旧克隆会把父提交一并打包而被拒。

写点 allowlist 守卫（`scripts/authz_write_points_guard.sh`）与其两个自测变异（`--selftest-add` / `--selftest-remove`）是三道独立门，任何未登记的主干 ref 写入口都会使守卫非零退出。

## Kill Switch 骨架与安全写原语（UN-33；REL-02）

运维入口：`bash scripts/authz_kill_switch.sh`（生产与 fixture 同一文件；家族内后续卡在同一 `--selftest` 入口叠加门）。

- **分派**：`--branch systemd|compose|file`（目标分别取自 `KILL_SWITCH_ENV_FILE` / `KILL_SWITCH_COMPOSE` / `KILL_SWITCH_CONFIG`）。无 `--apply-content` 时走 UN-41 变换；有则仅为骨架/元数据低层替换。可选 `--restart -- <argv...>`（UN-46：argv 数组恰好一次执行，禁二次分词）。启动时先跑 UN-50 preflight；重启成功后若设置 `KILL_SWITCH_URLS` 则跑 UN-36 HTTP 探测。
- **preflight（UN-50）**：Linux 专属；校验 cp/yq/jq/rg/flock/stat/getfacl/getfattr 存在性、GNU `cp --preserve=all`、`KILL_SWITCH_BIN authz-audit fsync --probe`、版本下限（coreutils≥8.30 / yq≥4.18 / jq≥1.6 / rg≥13 / util-linux≥2.27）；任一缺失/不足 fail-closed 并给出安装指引。
- **HTTP 探测（UN-36）**：`KILL_SWITCH_URLS` + `KILL_SWITCH_EXPECT_CODE` + `KILL_SWITCH_DEPLOY_HTTP`；canonical origin 绑定（默认端口/IPv6）；https 强制证书链校验；失败终态 T2 → 退出码 6、配置保持 off；**不自称恢复绿**。
- **evidence 记录（UN-53）**：探测前经 `KILL_SWITCH_BIN authz-audit run-init --restricted-root "$KILL_SWITCH_RESTRICTED_DIR"` 分配 run；脚本自持 `runs/<run-id>/.lease.lock`（长生命周期，≠ UN-56 `.evidence.lock`）；逐项经 `evidence-append`（channel/check/verdict[/status]）写入，禁止脚本内拼 JSON；成功 `run-commit`（`RUN_CAP`），异常 `run-abort`；256 KiB 硬上限与未知枚举拒绝由 CLI 透传。
- **日志通道（UN-47）**：`KILL_SWITCH_LOG_DIR`；重启前按 inode 捕获字节游标；重启后有界轮询落盘（默认 30×2s）；只读增量；`rg would-deny` 按 0/1/>1 分支（>1 硬失败、不转零命中）；零新增则 `log_no_would_deny=pass`，否则 T2。
- **Git HTTP 只读（UN-42）**：`KILL_SWITCH_GIT_REMOTE` + `KILL_SWITCH_DEPLOY_GIT` + `KILL_SWITCH_GIT_ASKPASS`；`git ls-remote`（凭据仅经 ASKPASS，禁 argv）；canonical origin 绑定；stderr 脱敏；可选 `KILL_SWITCH_GIT_ZEROCHECK_REPO` 断言 refs/objects 零变化。
- **SSH 只读与跨通道绑定（UN-44）**：`KILL_SWITCH_SSH_HOST/PORT/USER/KEY` + `KNOWN_HOSTS` + `REPO` + `DEPLOY_SSH`；启动时三通道 early bind（不符 → 退出码 7、零写入）；pinned known_hosts；`git ls-remote` over SSH。
- **变换与读回（UN-41）**：systemd 将 `MEGA_CEDAR__ENFORCEMENT=` 替换为 `=off`；compose 仅 mapping 形态经 `yq` 写 `"off"`（list/缺键 fail-closed）；file 改写 `[cedar].enforcement` 为 `"off"`（缺键 fail-closed），必选 `MEGA_PROFILE`，并以 UN-01 `config validate --show-sources --format json` 断言 winning source 为该文件（环境源获胜则禁用）。落盘后恰好一次读回校验为 `off`（`KILL_SWITCH_READBACK_FAIL=1` 可注入）。
- **写原语**：同目录临时文件 `O_EXCL|O_NOFOLLOW` → `cp --preserve=all` 播种 → 原地改写字节 → 元数据逐项校验 → `KILL_SWITCH_BIN authz-audit fsync <tmp>` → `renameat` → `authz-audit fsync <target>`；目标须为 no-follow 普通文件；systemd EnvironmentFile 拒绝重复键。
- **重启与失败终态（UN-46）**：`--restart -- <argv...>` 恰好一次；T1 重启失败 → 退出码 4、配置保持 off、打印人工重启指引；T3 rename 后 fsync 失败 → 退出码 5、不回滚为 on、重跑幂等收敛。
- **元数据保持（UN-48）**：owner / mode / ACL / xattr；写后 `stat`/`getfacl`/xattr 比对，不符即失败（可用 `KILL_SWITCH_META_VERIFY_FAIL` 注入）。
- **自测**：`bash scripts/authz_kill_switch.sh --selftest`（… + UN-42×6 + UN-44×7 = **58/58**；fsync stub = `scripts/authz_kill_switch_fsync_stub.sh`）。需可用的 `KILL_SWITCH_BIN` 或 `target/debug/mega2`；compose/preflight 需 `yq`（或 `KILL_SWITCH_YQ`）；缺 `getfattr` 时自测会在临时 PATH 放入存在性 stub。
- **发布**：REL-02 十子卡本地提交后由 UN-45 原子发布为 **v0.2.65**（脚本入口 / 三分支 / 拓扑绑定 / 失败终态安全方向见上；降级 = 不执行该脚本）。

## 迁移覆盖：`mega_cl.link` 唯一索引（UN-10）

`m20260815_000000_unique_mega_cl_link` 为 `mega_cl.link` 建唯一索引 `idx_mega_cl_link_unique`（既有索引只有复合的 `(path, link)`，`link` 单列查询既非索引等值探测也不保证唯一）。`up` 在同一事务内先取 `LOCK TABLE mega_cl IN SHARE ROW EXCLUSIVE MODE`，再扫描重复 `link`；发现重复即中止并在错误里列出全部冲突 link 与各自行数，不修改任何业务行。锁从扫描前持有到索引建成，杜绝「扫描通过后、建索引前插入重复」的竞态。

恢复为 forward-only：运行时 runner 只暴露 `up`/`refresh`，已发布索引只能由新迁移前滚修复；`down` 仅供隔离测试与开发 `refresh`，不得作为生产回退。

迁移模块内 `#[cfg(test)]` 覆盖四条：`up` 建唯一索引且 `down` 删除、重复数据使 `up` 带完整冲突清单失败且行数不变、并发插入在迁移事务提交前被锁阻塞（提交后被唯一索引拒绝）、`SET LOCAL enable_seqscan = off` 下 `EXPLAIN` 的计划命中新索引名。`get_cl` 的按 link 回归在 `src/jupiter/storage/cl_storage.rs` 内。

## 迁移覆盖：视图表（HP-09）

`m20261006_000100_add_view_tables` 按顺序创建八张无外键的表：
`mega_view_filter`、`mega_view`、`mega_view_root_chain`、
`mega_view_root_chain_scan`、`mega_view_commit_map`、`mega_view_object`、
`mega_view_object_ref` 与 `mega_view_register_log`。最后一张表及其
`(requester, created_at)` 索引位于迁移的最后一组语句，确保故障注入能
验证整个迁移的事务边界。

恢复是 forward-only。运行期只有 `up` 与开发用的 `refresh`，`down` 是空实现；
已发布的 DDL 问题必须用后续迁移前滚修复。升级时服务启动会自动执行待处理
迁移；未启用视图的部署会保留这八张空表。

`jupiter::migration::tests` 中的
`view_tables_match_design_schema`、`view_tables_have_no_foreign_keys`、
`view_tables_migration_up_replays_over_existing_schema`、
`view_tables_leave_existing_schema_unchanged`、`view_entities_round_trip` 和
`view_tables_migration_rolls_back_and_recovers` 覆盖完整列、可空性、默认值与
索引集合；没有外键；对已有 schema 重放 `up` 的空操作；新迁移不改变既有
schema；八个 SeaORM 实体各写入后按主键读回；以及预建同名
`mega_view_register_log` view 使最终索引失败时，八张表和迁移记录全部回滚，
移除冲突对象后可直接重跑。

## DB 模块覆盖：视图根链（HP-10）

`ViewStorage::extend_root_chain(budget, batch_size, lock_mode)` 在短事务中持有
根链锁 L_C，返回 `CaughtUp`、`NotCaughtUp` 或 `RolledBack`、`Forked`、
`UnrelatedHistory`、`MultiParent`、`MissingFirstParent`、`RowConflict` 六种不连续原因。扫描提交的首父链
先落入 `mega_view_root_chain_scan`，再按 `seq` 递增分段写入
`mega_view_root_chain`；每段把插入和锚点切换放在同一个事务中。预算同时计算暂存
写入与根链插入，每批回走和每段接入都按剩余预算截断；锚点判定不写行，也不耗
预算。耗尽时保留锚点供下次继续。`insert_segment` 在调用方的段事务内
执行；主键冲突核对失败时，调用方回滚整个段，因此不会留下该段已先插入的部分行。
不连续事务不改根链表，同一调用先前已经提交的段保持有效。

锁键使用 `VIEW_LOCK_NS`、`VIEW_FILTER_LOCK_NS`、`ROOT_CHAIN_KEY`、
`OBJECT_GC_KEY` 与 `REGISTER_KEY`；段上限是
`ROOT_CHAIN_SEGMENT_ROWS = 10_000`。`hash32(x) = ((x as u64) ^ ((x as u64) >> 32)) as u32 as i32`
折叠 64 位 id 的高低两半。
测试 schema 把第二键映射为 `hashtext(current_schema() || ':' || d)`，使并行 schema
不相互阻塞。`Blocking` 模式以绑定的 `set_config('lock_timeout', $1, true)` 设置
`VIEW_LOCK_TIMEOUT`（两秒）等待上限；超时按 SQLSTATE `55P03` 返回未追上，取消等
其他数据库错误原样上抛。返回 `Ok(false)` 的事务已经 aborted，调用方只能回滚；
成功设置的 `lock_timeout` 一直持续到事务结束。取锁语句由 `view_lock_stmt_prod` 与
`view_lock_stmt_test` 单独拼装，语句本身不带 `lock_timeout`；需要不限时等待的调用方
可直接执行 `Blocking` 语句。

`ViewStorage` 聚合在 `AppService` 中，访问器返回克隆而不新建实例，以便同一进程
共享告警记录。它的 `base` 字段保持私有；同级实现经 `Deref<Target = BaseStorage>`
取得连接。`view_test_fixtures` 以 `pub(crate)` 提供参数绑定的路径到 blob 字节和
预构造 tree 列表、线性/多父/首父缺失/无关根历史，以及可回退的 root `main` CAS
夹具；它写入 tree 层、返回 blobs，但不写 blob。

六个 lib 用例分别覆盖：线性冷启动和增量；预算耗尽后的续接及回走中途重启；分段
锚点、每段后的重启、两个连接池按 Try/Blocking 换手，以及扫描期间 `main@/` 前进；
五类根历史不连续、三种 `insert_segment` 既有行输入和第二段冲突；不成功的
Try/Blocking/取消等锁；以及全部锁键与同 schema 的两两隔离。
不连续事件仅从 `view_root_chain` 发出，带 `reason` 与 `commit_id`，按进程中的
`(reason, commit_id)` 去重，并在 `CaughtUp` 后清除记录。带数据库的事件捕获在
单线程 Tokio 测试中安装按该模块 target 过滤的线程局部 subscriber，同时持有第二个
`tracing::Dispatch`，避免并行测试首次命中 callsite 时缓存 no-op interest。取消等锁
用持锁事务之外的自动提交查询在一秒内轮询 `pg_stat_activity`；事务快照不会看到之后
才出现的等待者。代码审查点是根链只按 `seq` 递增插入，绝不以 `max(seq)+1` 追加单行。

## DB 模块覆盖：视图注册准入（HP-13）

`ViewStorage::admit(req, limits)` 在一个短事务中实现注册准入。`Register` 按
`filter_id` 查找定义，`Rewarm` 按主键查找；不存在的 Rewarm 返回
`MegaError::NotFound`。两种模式都在定义新建或处于回收态时判定冷启动，依次检查
活跃过滤器总数和冷启动名额。命名 Register 只读取该 name 的最新
`(version, filter_pk)`：同一活跃定义且不需新版本时返回 `Idempotent`，其余成功
请求返回 `Admitted`；`version` 是请求 name 的最终最新版本（无 name 时为
`None`），`ready` 是第 2 步读到的 `ready_seq` 是否非空。第 3 步的速率窗口和
`mega_view_register_log` 读写由 HP-31 接入，本卡不读写该表。

`AdmitRequest`、`AdmitMode`、`FilterDefinition`、`AdmitLimits`、
`AdmitOutcome` 与 `RejectReason` 都限定在 storage crate 内。限额仅来自调用方传入的
`AdmitLimits`：HP-13 的 `max_filters` 与 `max_concurrent_cold_starts`，以及 HP-31
追加的 `register_rate_per_token` 都由 `From<&ViewsConfig>` 读取；准入自身不读取
`Storage` 的配置快照。总数与名额拒绝返回
`Rejected { reason, retry_after: 30 秒 }` 并回滚；幂等命中没有写入。新过滤器以
`ON CONFLICT (filter_id) DO NOTHING RETURNING` 写入，若 L_R 以外的写入方在查询后
抢先提交，SeaORM 的零返回行会规范为
`MegaError::Db(DbErr::RecordNotInserted)`，事务回滚，绝不会用未插入的 id 创建
`mega_view`。

第 1 步直接执行 `admit_lock_stmt(cfg!(test))` 返回的 HP-10
`view_lock_stmt_prod` 或 `view_lock_stmt_test` 的 `ViewLock::Register`、`Blocking`
语句。该纯函数只负责委托拼装；准入不调用 `acquire_view_lock`，因为后者会设置两秒
`lock_timeout`，而设计 §6.5 要求 L_R 不限时等待。事务不取得其他视图锁，也不重写
§4.4 的锁键方案。

七个 lib 用例覆盖 `concurrent_last_slot`、`same_filter_concurrent`、
`rejection_and_idempotent_hit`、`checks_follow_design`、`cold_start_marks_warming`、
`named_register_versions` 与 `limits_follow_reload`。并发用例用 `test_db_config` 和
`database_connection` 建同一 schema 的多个连接池，由第三个连接的未提交事务持有
L_R，并轮询确认两个准入正在等待再释放；轮询失败分支会显式回滚 H，每轮的
`tokio::join!` 外包 30 秒超时，延时轮分别确认两个请求都在释放 L_R 后才返回，因而等待
超过通常锁超时仍可成功。其余用例直接构造就绪、预热、部分投影和回收态行，以三表 JSON
快照验证拒绝、幂等、Rewarm 与错误都不写行；改名列注入第 6 步写失败，并让 L_R 外部
未提交的同键插入与准入经 `tokio::join!` 在 30 秒内运行，在观测到 transactionid 等待后
提交；观测失败先显式回滚写入事务，验证零行冲突回滚。

## DB 模块覆盖：视图注册速率窗口（HP-31）

`AdmitLimits` 从调用方的配置快照额外携带
`register_rate_per_token`，`RejectReason::Rate` 表示按 requester 的速率拒绝。
`ViewStorage::admit` 在 Register 的幂等判定之后、总数和冷启动名额之前，以同一短事务
读取 requester 在 `REGISTER_RATE_WINDOW_SECS`（3600 秒）内的注册行数和最早一行的
离窗秒数。达到阈值时显式回滚并返回向上取整、最多 3600 秒的 `retry_after`；成功的
计速率 Register 在提交前插入一条默认时间戳的日志，并仅删除该 requester 的过期行。
`Idempotent` 与 `Rewarm` 不读写 `mega_view_register_log`。

六个 lib 用例覆盖：`rate_rejects_iff_requester_window_full` 的三类计速率 Register、
过期行、requester 隔离和热加载阈值；`rate_rejection_writes_nothing` 的速率、总数和
名额拒绝逐行三表快照；`rate_retry_after_from_oldest_row` 的 SQL 时间边界和 3600 秒上限；
`rate_admitted_appends_and_trims` 的插入与 requester 范围内清理；
`rate_idempotent_and_rewarm_not_counted` 的不计数路径；以及
`rate_concurrent_last_quota` 的跨副本最后一个速率配额。窗口状态由直接插入的
`created_at = localtimestamp - k 秒` 行构造，并在各状态之间清空运行态表；重试秒数由
调用前后的 SQL 值夹定，Rust 不直接比较时间。并发用例用 `test_db_config` 和
`database_connection` 共享 schema，先预热三个连接池，再由第三个连接以
`ViewLockMode::Blocking` 持有 L_R；只有观察到两个等待者才释放锁，10 秒内未观察到则
显式回滚，整个 `tokio::join!` 另有 30 秒上限。

## DB 模块覆盖：根链停追与恢复（HP-28）

`ROOT_CHAIN_HALTED_SQL` 是不带绑定参数、不引用外层别名的完整布尔表达式。读者可把它
原样嵌入自己的单条快照查询；`ViewStorage::root_chain_halted()` 只执行一条
`SELECT`，从不取得根链 advisory lock。它依[事实校准第 7 项](history-projection.md#事实校准2026-10-05)的顺序先判断暂存表顶行是否已在
根链中：该行只有不再是链尾时才停追；不在根链中时才依次判定无父而链表非空、多父、和
首父缺失。因此，暂存表为空、已追平，以及无父的 bootstrap 仍是链尾或冷启动尚未接入时
都不会误报停追。

暂存表顶行持久化了五种根历史不连续的判定。后续 `extend_root_chain` 在锁内只复核该顶行，
不改根链表或暂存表；语句数与历史长度无关。把 `main@/` 恢复到旧链尾的单父后代后，删除
暂存表即可重新追赶；若不恢复 `main@/`，相同操作会重新得出原来的不连续原因。

`view_root_chain` 的七个 lib 用例覆盖：五类不连续经同 schema 的新连接池重启后复现；8 行
与 256 行历史的停追调用等语句数和单语句谓词；停追后 `main@/` 前进仍不写两张表；另一个
连接池在根链锁被持有时仍读取停追；从未扫描、追平、预算扫描、段间锚点、bootstrap 与冷
启动等正常状态均为假。分段接入期间再在同一条快照 SQL 中读取谓词和两张表的行数，
并断言谓词为假；恢复后清空暂存表会重新追赶；未恢复时清空暂存表会再次停追。需要第二个
连接池或重启的用例通过 `test_db_config` 保持同一 schema，并用
`database_connection` 创建新的连接池。

## DB 模块覆盖：视图 tree 读取（HP-29）

`MonoStorage::get_commits_by_hashes_fallible` 与
`MonoStorage::get_trees_by_hashes_fallible` 先对请求 id 去重，再以 1000 条为一组在给定
连接或事务上读取；查询错误以 `MegaError` 返回，数据库中没有的 id 不会被误报为查询错误。
`ViewTreeSource` 只在异步 `prefetch` 中访问数据库：它先读 `mega_view_object(kind = 2)`，
再用剩余 id 读 `mega_tree`，并且只在两个阶段都成功后写入本实例的已预取与缺失集合。同步的
`TreeSource::read_tree` 不发 SQL；已确认缺行、坏字节和未预取 id 分别给出 `Absent`、
`Malformed` 与 `Unprefetched`。EMPTY_TREE 不进入预取集合，由 HP-04 的 `read_tree` 直接处理。

六个 lib 用例是 `fallible_batch_reads_rows_then_query_error`、
`fallible_batch_reads_chunk_statement_count`、`lookup_order_view_object_first`、
`read_tree_outcomes_over_db_source`、`prefetch_statement_count` 和
`prefetch_query_error_keeps_state`：它们分别覆盖连接和事务上的去重、缺行、分块和查询错误；
视图对象优先于 L0 tree、kind = 1 回退、显式 SHA-1/SHA-256 解析及事务构造；三种
`MissingObject` 原因和补行后的新实例读取；`prefetch` 的 1000 条分块、缓存命中与同步读取
零查询；以及在测试 schema 内改列名后，预取失败不污染既有缓存或把新 id 误记为缺失。
语句计数用 `test_db_config` 自建连接并安装 metric callback，避免替换
`test_db_connection` 持有 schema 守卫的回调。失败注入只改列名，不改表名，也不用
`DatabaseConnection::default()`。每个批量查询的语句数为 ⌈n / 1000⌉；HP-11 的语句数断言
按同一公式计算。调用方收到 `Unprefetched` 说明漏做预取，它不表示对象缺失。

## DB 模块覆盖：视图投影追赶（HP-11）

`ViewProjectionService` 在单个事务中按 `filter_pk` 读取过滤器、复核规范定义，取得连续
根链批次，并经 `ViewTreeSource` 按层预取 tree。它依次尝试过滤器锁与对象 GC 共享锁；未准入
或试锁失败不写入。批中 `seq = s0` 只提供父 tree，不重复投影；每个后续根链行直接使用前一行
的 `tree_id`，不会为每个提交另查父链。成功批仅为实际写出视图提交的段写提交对象、生成 tree
与引用，以 `GREATEST` 推进水位；最后一批在同一事务内检查 `main@/` 覆盖并标记就绪。预取后
同一 tree id 仍报告未加载会作为内部错误终止，避免循环重试。

`jupiter::service::view_projection_service::tests::determinism`、
`jupiter::service::view_projection_service::tests::commit_map_matches_design`、
`jupiter::service::view_projection_service::tests::object_refs_match_design`、
`jupiter::service::view_projection_service::tests::ready_same_txn`、
`jupiter::service::view_projection_service::tests::noop_returns`、
`jupiter::service::view_projection_service::tests::failed_batch_no_effect` 和
`jupiter::service::view_projection_service::tests::statement_count_independent_of_batch`
覆盖批次收敛、映射游程、对象与引用、同事务就绪、无操作返回、前提失败与内部错误，以及批量
路径。映射用例用内存 `TreeSource` 加逐提交 `project_commit` 形成独立参照，覆盖起始空段、未预取
重算和父提交为空的首段；对象用例先单批持久化起始空段，确认该段没有对象或引用。夹具根提交以固定
author/committer 签名和时间戳构造，因而可跨 schema 比较投影字节。
`determinism` 覆盖 Subdir、Prefix、Exclude 与 Compose：以一个 B=1000 基准结果比较两个独立
schema 的冷启动、清表重建、同一过滤器上共享服务实例克隆的并发追赶（遇 `NotRun` 重试）、B=1/B=1000，
以及批间钩子调用 `ConfigHandle::reload` 将 `views.batch_size` 从 3 改为 2；钩子记录实际水位 3、5
与应用字段 `views.batch_size`，防止热改用例退化为单一批大小。

服务实例持有同一份 memo 与计数器；测试构建下的钩子记录 memo 命中、补预取次数、每批预取的
id 集合、已提交批的水位和终态 `s0 >= tip` 检查，并能在试算和段计算之间清空 memo、在批间触发
回调。对象引用用例会删去仅由 Exclude 视图引用的对象并复位该视图，再以同一服务重跑，核对 memo
命中后补写的对象字节、类型和引用行；共享的 EMPTY_TREE 则保留并核对 `gc_marked_at` 被清空。
TreeSource 钩子可模拟漏记未加载与持续未加载，分别覆盖未经过包装层的读取和补预取后仍报告未加载的
fail-closed 路径。补预取的生产来源是 seq=1 的 `is_empty_root` 递归与 HP-06 登记的 R8 例外；
试算产出的 `FilterOutput` 直接交给 `project_with_output`，因此 memo 在两阶段之间被清空也不会使
段计算重新执行 `filter_tree`。语句数用 `test_db_config` 取得独立 schema，并以
`Storage::new_with_connection` 在自建连接上安装 metric callback；B=10 与 B=500 的常规档各断言一
个推进事务和一个终态事务，预热 memo 后强制清空的两档各在新 schema 比较，后两档不与前两档比较。
`ready_same_txn` 还让尚未 ready 的过滤器追到链尾时发现 `main@/` 在链外、再回退到链尾，验证
`s0 >= tip` 分支首次写入 `ready_seq`；另以新过滤器验证扩展根链后的批内写入路径。

批事务每次重新调用 `recheck_definition`；不存在的过滤器行和 `DefinitionCorrupt` 都按内部错误
回滚。`failed_batch_no_effect` 是同步测试，`capture_tracing` 在 current-thread runtime 内驱动异步
调用，保持 `_pin_registry` 存活并使用 `with_ansi(false)` 捕获 ERROR 事件。它以 H=6 的 Prefix 夹具
逐一验证 B=0（s0=0、s0=2）和根链空洞三种前提失败；内部错误的七个子例为：过滤器缺失、定义往返
损坏、定义哈希损坏、Compose 冲突导致的 `ProjectFailure::Internal`、不记录未预取、持续未预取，及在
测试 schema 内把 `mega_view_object_ref.object_id` 改名造成的查询失败。每次调用都单独核对新增的一条
ERROR 事件、状态回滚、指标与脱敏字段；列名恢复后，同一服务可重新追平并与独立 schema 的参照快照
逐字节比较，证明失败事务不污染水位或映射；`ON CONFLICT DO NOTHING` 的整批冲突也通过无返回值插入
执行，不会把合法对象复用变成失败。

## DB 模块覆盖：视图投影停止（HP-12）

`ViewProjectionService` 在单批事务中把 `ProjectFailure::Data` 交给纯停止分派：
前提校验失败、缺失 tree 行、无法解析 tree 字节和缺失 commit 行分别以
`premise_check_failed`、`row_absent`、`unparsable`、`commit_row_missing` 停止；
`MissingObjectReason::Unprefetched` 仍按内部错误回滚。停止点为 `s` 时仅写入
`seq < s` 的段、对象和引用，将 `projected_seq` 推至 `s - 1`，不标记 ready。提交成功后才
递增 `view_projection_stops_total` 并发出一条 ERROR 事件。事件字段为 `metric`、`filter_id`、
`s`、`commit_id` 和 `reason`；缺对象时另有 `tree_id`。字符串字段以 Display 记录。字段断言
只分析事件 target 之后的文本；任何 span 上下文都不算事件字段，且事件不回显作者、提交者或消息。

`jupiter::service::view_projection_service::tests::stop_state_equals_prefix_reference` 覆盖四种
故障在 B=1 与 B=1000 时的前缀状态、重复追赶和显式单批入口；
`jupiter::service::view_projection_service::tests::stop_alert_and_counter` 覆盖逐次 ERROR 告警、
脱敏和共享计数器；
`jupiter::service::view_projection_service::tests::non_stop_inputs_reach_ready` 覆盖被 J4 丢弃、
未读取缺失 tree 和真实不存在路径；
`jupiter::service::view_projection_service::tests::stop_dispatch_excludes_not_prefetched` 覆盖
分派表；`jupiter::service::view_projection_service::tests::stop_resumes_after_repair` 覆盖补回 L0
行或字节后的续追；`jupiter::service::view_projection_service::tests::stop_changes_only_projected_seq`
覆盖冷启动及已就绪状态只修改水位；
`jupiter::service::view_projection_service::tests::cold_start_slots_gauge` 覆盖快照和冷启动名额。

夹具先建立 12 个固定哨兵签名与 message 的根提交和根链，在第 7 个提交制造四类故障；P 组以带前导零
author 时间戳的原始字节经 `Commit::from_bytes` 和 `into_mega_model` 写入，所有组的作者与 message
哨兵为 `hp12-sentinel-author` 和 `hp12-sentinel-message`。独立 schema 的前六个根提交提供前缀参照；
缺行和缺提交修复时恢复原模型，损坏 tree 恢复原字节。语句计数通过 `test_db_config` 和
`database_connection` 自建连接，并以 `Storage::new_with_connection` 复用该连接；metric callback
只记录 `mega_commit` 的 SELECT。多个 schema 可以共享同一份 `ViewMetrics`，因其计数由 `Arc` 维护。
告警捕获使用带 `_pin_registry` 的、闭包内可读取缓冲的 DEBUG helper，按每次调用前后的差值断言；
查询失败在测试 schema 中把 `mega_view_object_ref.object_id` 改名而非改表名。
`ViewMetrics::counters()` 只读取原子总数；`ViewMetricsSnapshot` 以 `#[serde(flatten)]` 展平它们，
`metrics_snapshot()` 另以一条 `warming_since IS NOT NULL` 计数查询填入
`view_cold_start_slots_in_use`，并在每个快照步骤与同一原生 SELECT 的结果比较。

## DB 模块覆盖：视图 worker（HP-14）

六个 lib 用例是 `view_worker_spawned_only_when_enabled`、`worker_hot_reload`、
`round_selection`、`compensation_liveness`、`round_errors_do_not_stop_worker` 与
`view_runtime_wiring`。它们覆盖启动时 `[views].enabled` 门控、正在执行的轮被取消、
热加载、候选选择、周期补偿、候选与整轮错误隔离，以及共享运行时的作用域。视图 trunk
配置使用 `push_policy = "trunk"`、`push_auth = "none"`、`ssh_receive_pack = false` 与
`cedar.enforcement = "off"`，并显式启用 views。

`spawn_view_worker_with_round` 是测试替换轮函数的接缝，`production_round` 是取得生产轮的
唯一入口。每轮先读配置快照、试锁扩展根链、查询候选，再按主键顺序调用共享服务的
`catch_up`；每批的 `batch_size` 仍由 `catch_up` 自行读取，所以热改在下一批生效。候选
失败只记录错误并继续其余候选；根链或候选查询失败只记录该轮错误，下一 tick 继续。测试
通过 `compensation_round` 的逐候选函数注入错误或记录候选集合。

`ViewRuntime` 随一个 `Storage` 及其克隆共享 `ViewMetrics`、`ViewSignal` 和一次性的
`OnceLock` 服务槽。`view_projection_service()` 传给服务的内部 Storage 使用空槽，避免
服务反向持有同一槽产生 Arc 环；因此服务、memo 和计数器在同一进程 Storage 内共享，而
测试 schema 的连接仍可释放。`notify_worker()` 用 `notify_one` 保留许可；HP-32、HP-19、
HP-33 和 HP-21 分别消费该信号接口。

worker 只在启动时 enabled 才注册热加载订阅者。订阅者只持有 watch 控制结构，不捕获
Storage；收到 `views.` 应用字段后转发新的 `ViewsConfig`，`worker_interval_secs` 改变时以
`.max(1)` 重建 ticker。循环把进行中的轮与 `token.cancelled()` 放在同一个 `tokio::select!`
中，取消时丢弃本轮而不等待无上界的冷启动；短事务、根链段和投影批会各自回滚或保留已
提交状态。后台根链扩展传入 `budget = None`，不受 `max_append_walk` 限制，保证超过前台
上限的根链仍在后台追平。

`worker_hot_reload` 与 `compensation_liveness` 的双连接池情形使用 `test_db_config` 和
`database_connection` 建 worker、持锁两个连接池，持锁事务不占 worker 的池连接。
`worker_hot_reload` 以 `mega_view_filter` 的表锁让首轮已取旧配置快照、但还未开始第一批，
再热改 `batch_size`；根链缺口使新批大小 1 的第一批推进到 seq 3，区别于缓存旧值的结果。
`compensation_liveness` 覆盖冷启动在 seq 1 中断后由另一个 worker 继续、链尾水位但尚未就绪的
过滤器、持有视图锁后释放且不发送信号，以及超过 `max_append_walk` 的根链在后台无预算追平。
故障注入只改列名，不改表名，避免 `search_path` 回退到 public 表。`start_http` 以
`ctx.storage.clone()` 启动 worker，并在关闭时以 30 秒上限等待任务结束。

## 迁移覆盖：`merge_queue.requester` nullable 列（UN-18）

**历史表。** [`plan-20260910.md`](../plan/plan-20260910.md) MW-05 已 `DROP TABLE merge_queue`；下列描述 UN-18 当时的加列迁移，不再是现行 schema。

`m20260815_000100_merge_queue_requester` 为 `merge_queue` 增加 nullable `requester` 列（无默认值、无回填、不修改既有行）。排队的合并由后台 worker 稍后执行，请求主体必须随队列项持久化，否则执行时没有自己的授权主体。本卡只加列：写入/读出链归 UN-20，NULL（legacy 行）的执行判定归 UN-17。

**恢复路径按消费面状态区分（forward-only，运行时无 down 入口）：**

- **pre-consumer**（UN-20 未发布，列恒为空）：可由新迁移前滚删列。
- **post-consumer**（列已在写入 requester）：**禁止无条件删列**——会丢失授权主体与审计数据。只能保留列并停用消费方，或以新列替代并复制数据。

迁移模块内 `#[cfg(test)]` 覆盖：列为 nullable 且无默认值、既有行读回 NULL、隔离库内 `down` 删列后 `up` 可前滚回已发布状态。

> 写迁移测试时注意：单元测试库按 **schema** 隔离（`search_path`，见 `src/jupiter/tests.rs`），而 `information_schema` / `pg_indexes` 跨全部 schema，因此目录查询必须带 `current_schema()` 限定，否则会读到并发用例的表。

## merge queue requester 读写链（UN-20）

`/merge-queue/add` 与 `/merge-queue/retry/{cl_link}` 经 `OptionalSessionUser`（UN-22）捕获**可选**主体：该 extractor 永不 401，因此匿名入队/重试仍可服务，记为 NULL——本卡不产生任何新的拒绝面。

链路：router → `MonoApiService::{add_to_merge_queue_as, retry_merge_queue_item_as}` → `MergeQueueService::{add_to_queue_with_requester, retry_queue_item_with_requester}` → storage。requester 与队列行**在同一条插入/更新语句**里落库——排队的合并稍后由后台 worker 执行，行存在而主体缺失就是一次没有主体的合并。

既有 storage 方法签名零变更：`add_to_queue` / `retry_failed_item` 保留并委派。语义差别值得记住：`retry_failed_item`（不知道主体）**不动**已记录的 requester，而 `retry_failed_item_with_requester(link, None)`（显式匿名）会把它清空。

读出面 `get_requester(cl_link) -> Option<Option<String>>`：外层 `None` = 没有该队列行，内层 `None` = 匿名或 UN-18 之前写入的 legacy NULL 行。执行时的主体判定由 UN-17 消费该读出面。

> 单测提示：`jupiter::tests::test_storage` 的 `merge_queue_service` 原本是 `mock()`（disconnected 连接），驱动队列链路会 panic；已改为与该 storage 共用同一测试库。

## CI

`.github/workflows/config-validation.yml` runs formatting, Clippy, the
compose-backed integration targets (including `integration_website_auth`
under `WEBSITE_IT=1`), and the real website session check after checking
out the `website frontend` sibling. Mega2 notification paths do not require a local
SMTP dependency.

## 只读装配的零副作用比对（UN-30 / UN-43）

`integration_authz_audit` 的立论方式是**前后对照**，不是通读代码。播种走**真实二进制**（`mega2 service http` 起来再 SIGINT），因此迁移、`init_monorepo()` 写下的 refs 与对象全都真的发生过——手搓出来的库只包含我们想到的东西，而「零写入」恰恰是关于没想到的那些。

快照有七个面：schema（范围是除系统 schema 外的**全部** schema）、每表**内容摘要**（整行转文本排序聚合后 md5，而不是行数——一次 UPDATE 不改变计数）、`pg_sequences` 当前值（被回滚的插入不留行却推进序列，那同样是一次写）、`mega_refs` 全行、三张对象表全行、对象存储目录逐文件散列，以及 `MEGA_BASE_DIR` + `MEGA_CACHE_DIR` 的逐文件指纹（vault 的 `core_key.json` 就在这一面里）。文件指纹是**内容散列 + 尺寸 + 权限位 + mtime**：一次「内容相同」的重写不会改变内容散列，却仍然是一次写，只有 mtime 能发现它。目录本身也收（路径 + 权限位），因此创建/删除/改名目录与只改目录权限都看得见。

用例分两幕：

1. **本地对象存储**：只读装配读根 ref 与授权源（这条路径同时用到 DB 与对象存储），前后七面全等。
2. **需要 vault 的部署**（UN-43）：先用 `config secret set` 把两个对象存储凭据写进 vault，再切到 s3compatible，于是只读装配**真的**打开 vault。断言它拿到的是 UN-31 的只读句柄、`denied_writes()` 为 0，且 vault 表与 core key 文件一字未动——bootstrap 路径会轮换 runtime 凭据并回写 key 文件，那正是这一幕要排除的东西。用本地存储时 vault 根本不会被打开，「Vault 状态零变化」会退化成一句空话，所以第二幕不能省。

最后做两次**量具校准**：插入一行探针（快照必须变）、再原地改写同一行（行数不变、摘要必须变）。否则「前后相等」可能只是因为这份快照什么都没量到。

受限根目录产物的排除项写在 `RESTRICTED_ROOTS` 常量里，今天为空；留着是因为「排除了什么」必须是一份明写的清单，而不是某处 diff 里悄悄少掉的几行。匹配用**路径前缀**而不是子串——受限根目录是一个路径祖先，子串匹配既会误伤中间路径同名的文件，也会漏掉本该排除的；且比的是**相对于受监视根**的路径（受限根目录就是这样登记的），拿拼好的绝对路径去比前缀永远对不上，排除清单会变成一个从不生效的摆设。

**这个 target 只有一个用例，且必须 `--test-threads=1`**：它用 `std::env::set_var` 把数据库与对象存储指向本用例专属的库（`.env.test` 里的 `MEGA_DATABASE__DB_URL` 指向共享 admin 库，而环境变量优先级高于配置文件）。这条约束不是靠注释请求后来者小心，而是**运行期强制**：第二次构造 fixture 直接 panic 并说明原因——两个 fixture 会互相覆盖对方的数据库地址，而且谁都不会报错，于是比对会静悄悄地跑在别人的库上。

**这一面量得到什么、量不到什么**：快照收录的是「最终存在的条目」，因此**创建后又删除**的临时文件不会被发现；两个受监视根目录之外的写也不会。前者是快照法的固有边界（要抓它需要的是事件流而不是快照），后者是登记范围的选择。两条都写在这里，是因为一份不说明边界的「零副作用」证明会被读成比它实际更强的东西。

## DB 模块覆盖：C 段信号与推送驱动投影（HP-32）

`PushQueueService` 在已提交轮次的 C 段索引结束后只向所属 `Storage` 的
`ViewSignal` 写入一个 `Notify::notify_one` 许可。该动作不等待 worker，也不查询
`mega_view_*` 表；索引成功、被后续同路径轮次跳过和索引失败的补偿路径都保留这一
通知。句柄在 `PushQueueService` 构造时注入，随后不再变更。detach 中无根树的
`Done` 路径与已挂载 ImportRepo 只更新 refs 的 `Done` 路径均不经过 C 段，根不前进，
因此不发送通知。缺省关闭 `[views].enabled` 时没有 worker 消费许可。

`spawn_view_worker_with_round` 同时等待周期 tick 与同一信号。worker 正在执行一轮时
到达的多个通知由 `Notify` 合并为一个许可，因此当前轮结束后只追加一轮；worker 不会
给自己发送信号。信号是进程内机制，其他副本通过周期补偿追平。

覆盖入口（进程级用例需要 `docker compose ... --profile git up -d --wait`，且
`MEGA2_IT_SKIP_GIT_CLI` 不得设置）：

- `jupiter::service::push_queue_service::tests::c_segment_signal_after_done` 覆盖 push、merge、attach、跳过和索引错误；
- `c_segment_signal_no_view_sql` 记录 push 与 merge 的 SQL，断言没有 `mega_view_` 表访问；
- `c_segment_signal_never_blocks_round` 对 push、merge、attach 各自比较无 worker、worker 轮次暂停和任务终止时的有界 B3 返回；
- `jupiter::service::view_worker::tests::signal_permit_rules` 通过实例级钩子在补偿轮开始前暂停，并在 `per_candidate` 包装中记录真实 `catch_up` 返回值，验证许可合并及不会自行续发；补偿轮本身不变；
- `out_of_order_signals_numbering` 在三个独立 schema 中按不同顺序落地根提交并发送无内容信号，与第四个冷启动 schema 逐行比较根链及映射行；
- `integration_git_cli_view_push_driven_projection_rows` 以 3600 秒周期排除后续 tick，覆盖 N=1、N>1、源路径外和排除目录的推送；净零推送确认根链及映射行不变；
- `integration_git_cli_view_enabled_trunk_path_regression` 复用原有两个 trunk 用例的完整正文与断言，通过注册及关停前回调额外验证启用侧追平与 I3。两个进程级用例覆盖 `start_http` 启动 worker 与 SIGINT 关停等待 worker 的路径。

ER-05 检查点：sea-orm 的 metric 回调不记录 `execute_unprepared`，因此
`c_segment_signal_no_view_sql` 的 SQL 记录不能覆盖这类语句。push B3、merge B3、
C 段的生产路径及本卡新增生产代码均不调用它：`blob_path_index.rs`、
`push_queue_storage.rs` 无此调用，`push_queue_service.rs` 与 `mono_storage.rs`
的命中只在测试模块内。启用视图的物化链回归按 ADR-HP-11 重跑 N=1 三轮快进与
N>1 squash 对齐两例并增加 I3；其他缺省配置下的 trunk 用例仍由全量门覆盖。
