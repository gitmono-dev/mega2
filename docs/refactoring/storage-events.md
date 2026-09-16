# Storage-only 提交后出站事件

本文是 monoengine **storage-only** 形态下 `[storage_events]` 静态配置的事实源。产品边界与任务追溯见 [`../plan/plan-20260912.md`](../plan/plan-20260912.md)。

> **状态（WH-01/WH-09/WH-11/WH-03/WH-04/WH-05/WH-06/WH-07）：** 配置表面 + HTTPS HMAC 运输 + 启动 secret 绑定均已落地；已装来源 hook：Git B3 push 的 `repo.push`（WH-03）、OCI manifest 发布的 `oci.manifest.published`（WH-04）、LFS basic 实际上传的 `lfs.object.uploaded`（WH-05，presigned 直传为登记缺口），WH-06 挂上 FastCDC media finalize 的 `lfs.media.finalized`，WH-07 挂上 Agent Capture 批提交的 `agent_capture.events.committed`。Agent checkpoint hook（WH-08）尚未安装。HMAC `secret_ref` 在 disabled 时不解析。

## 配置

`[storage_events]` serde 缺省：

| 字段 | 缺省 | 作用 |
|---|---|---|
| `enabled` | `false` | 出站开关；启用要求 storage-only |
| `installation_id` | 省略 | 启用时必填；1..64 ASCII `[A-Za-z0-9_-]`；部署侧生成并持久写入，重启不得变 |
| `max_in_flight` | `16` | 后续运行时在途上限（本卡只装载） |
| `connect_timeout_seconds` | `2` | 连接超时，范围 1..=5 |
| `request_timeout_seconds` | `5` | 含 DNS 的整段请求超时，范围 1..=10 |
| `shutdown_grace_seconds` | `5` | 后续关停宽限（本卡只装载） |
| `targets` | `[]` | 静态接收者；enabled 且为空合法（无接收者） |

`[[storage_events.targets]]` 在该项存在时必填 `id`、`url`、`secret_ref`、非空 `events`。`id` 为 1..32 ASCII `[A-Za-z0-9_-]`，不得重复。过滤数组缺省为空（空集合表示该来源不订阅，不是通配）。`url` 必须是 HTTPS，禁止 userinfo / query / fragment（disabled 同样拒绝非法形状）。事件字面量与 canonical 路径由后续卡校验。

## 运输（WH-09，未注入应用）

`HttpsEventTransport` 对每个 target 单次 POST，不重试（含 429/5xx）。禁止跟随 redirect，禁用环境代理。签名头：

- `X-Mega2-Event-Id`
- `X-Mega2-Timestamp`（UTC Unix 秒）
- `X-Mega2-Signature: sha256=<hex>`

HMAC-SHA256 输入为 `timestamp` 十进制秒、`.`、实际发送 body bytes。密钥必须是已解析 `SecretString` 的 `hex:<even-hex>`，解码后 32..=256 bytes。生产客户端没有 HTTP/私网逃逸开关。日志只记 target id / event type / 结果类别，不含 URL、secret、请求响应体或 reqwest 原文。

## 有界运行时（WH-02）

`StorageEventEmitter` 由 `Storage` 持有，默认为 disabled。`try_emit` 在 spawn 前 `try_acquire`，在途不超过 `max_in_flight`；已关闭的 admission 一律返回 `dropped_closed`。每个发送任务的 `JoinHandle` 立即经 channel 交给唯一 lifecycle 任务登记；任务在正常结束、panic 或被 abort 时都通过 drop guard 上报序号，lifecycle 据此逐一 join 并记录按 target 的结果类别（`delivered_2xx` / 非 2xx / connect / tls / resolve / timeout / cancelled / panicked），不依赖下一次发射清理。`shutdown()` 原子关闭 admission、等待 grace，超时 abort 并 join 全部仍在登记的任务；可重复、并发调用，发起者被取消后 drain 由 lifecycle 任务继续。lifecycle 只持有 `Weak` 回引用；最后一个 owner 销毁时登记 channel 关闭，lifecycle 立即 abort 并 join 全部未完成任务后退出（不同步 join）。

## 启动 secret 绑定（WH-11）

`AppContext` 在 vault 就绪、storage 建成后（redis/notification/bootstrap 之前）执行绑定：`enabled=true` 时逐 target 解析 `secret_ref`（只允许 `vault://secret/config/<profile>/storage_events/targets/<id>/hmac#<field>`，与 `config validate` 同一校验），经 `VaultSecretResolver`（caller `startup:storage-events`）取出 `hex:<even-hex>` 密钥并编译 `EventTarget`，再以配置的 connect/request timeout 构造 `HttpsEventTransport`，经 `Storage::set_storage_event_emitter` 安装为唯一应用 owner。任一 parse/namespace/解析/编码失败即启动失败（`MegaError`）；解析值不写回 config snapshot，错误与日志不含 SecretRef URI 或 secret 值（resolver 错误已脱敏）。owner 安装后若构造尾部失败（redis、notification、monorepo bootstrap 等），返回错误前先 `shutdown().await` 该 emitter（幂等；WH-13 外层尾段在正常退出时再调一次）。disabled 时完全不解析，保留默认 disabled emitter。

运维命令同步扩展：`config secret set/check/rotate` 接受 `storage_events.targets.<id>.secret_ref` 字段，vault-path 必须是 `config/<profile>/storage_events/targets/<id>/hmac`；set/rotate 只从 stdin 读值（`--value-stdin`，`printf '%s' "$HMAC" | monoengine ... config secret set ...`），轮换后必须重启服务（无动态热更新）。

CLI 接线已由 WH-13 交付：长运行 service（`http` / `ssh` / `multi`）不再安装直接 `process::exit` 的 Ctrl+C handler。`AppContext` 持有共享 `service_shutdown` CancellationToken；CLI 在 config 加载**之前**就为 `service http|ssh|multi` 安装「只记录」的 Ctrl+C handler（`ctrlc` crate，写入静态 watch channel，进程永不被信号默认终止；一次性命令保留原 `process::exit(0)` handler），`service` 创建 context 成功后用一次性 forwarder 任务把该记录（含已落早的信号）转入该 token（sticky，信号落在任一 server 注册 handler 之前也不丢），forwarder 由清理尾段 abort 回收。context 创建成功后的全部退出路径（subscribe / reload watcher / 参数解析、启动与运行错误、Ctrl+C、正常返回）统一经过 `commands::service` 的异步清理尾段——先按既有优先级停止 reload watcher（原结果不被清理覆盖），再 `shutdown().await` emitter，仅在其完成后才输出 `storage_events_shutdown_complete` 日志（仅类别字段，无 URL / secret / body）。`start_http` 内联轮询 `Serve`（无内层任务：multi abort wrapper 即真正停止监听），主 select 同时监听 ctrl_c 与共享 token（服务外直接调用仍可用）；优雅 drain 有界（30s；超时丢弃 Serve 即停止 accept 循环，残留的连接任务属 axum 既有后台任务，其生命周期重构不在 WH-13 范围：admission 已关闭的 emitter 不受其影响，runtime 收尾时一并回收），serving / drain 超时 / 后台任务错误在全部清理（含 emitter drain）完成后按 serving > drain > 任务错误的优先级作为命令结果返回，干净关停仍 Ok。`service ssh` 等待 token 后 abort 并有界 join 无关停 token 的 SSH server（is_finished 快路径与 abort 后的 join 结果都会保留真实完成/错误，只有预期取消与 join 超时才返回 Ok）；`service multi` 中 ssh 子任务始终立即 abort + 有界 join，http 子任务一律经共享 token 优雅停止——token 触发或 ssh 先完成时给有界优雅窗口，超时再强制 abort + 有界 join；先完成方的结果为整体结果，token 路径保留真实子错误（http 优先于 ssh，abort 取消不算错误）。非服务命令与 `service init` 保持原退出行为（AC7）。进程级证据见 `tests/integration_storage_events_runtime.rs`。

## 来源适配：`repo.push`（WH-03，已交付）

Git B3 真实 `n>0` push 的 `txn.commit()` 成功后、C-segment 前发一次 `repo.push`：scope 只填 canonical `repo_path`，data 为 `push_id,operation_id,ref_name,old_oid,requested_oid,landed_oid`。event_id 按冻结规则派生（`sha256("repo.push\0"+installation_id+"\0"+repo_path+"\0"+operation_id+"\0"+landed_commit_id)` 前 16 字节、UUID v5 布局），同输入稳定、跨安装/仓库/操作/落地 commit 区分；队列 i64 id 不参与身份。协议 receive-pack 与产品 API 写（`land_api_tip_push`）共用同一提交点。净零轮次（`n=0`）、Done replay、attach/merge、CAS/fencing 失败与回滚均不发。投递失败不改变已提交结果。覆盖边界：B3 提交语义见 [`trunk-push.md`](trunk-push.md) 阶段 7。

## 来源适配：`oci.manifest.published`（WH-04，已交付）

OCI manifest 发布在对象 + manifest DB 行 + 可选 tag upsert 全成功后发一次 `oci.manifest.published`：scope 只填 `oci_repository`，data 为 `digest,reference,media_type,size`，`event_id` 为 UUID v4。发布失败、chunk/blob/mount、digest/tag/超限拒绝均不发；重复 PUT 可重复通知；投递失败不改变已提交的发布。详情与崩溃窗口见 [`oci.md`](oci.md)「发布出站事件」。

## 来源适配：`lfs.object.uploaded`（WH-05，已交付）

LFS basic 上传在本次请求实际 `put_stream` 成功后发一次 `lfs.object.uploaded`：scope 三个值全 null（basic 上传没有可信 repo/tenant），data 为 `oid,size,transfer="basic"`；只发给显式 `include_unscoped_lfs=true` 的 target。batch 建行、exists no-op、hash/size 失败不发；presigned 发放与直传不发（无回调，覆盖缺口 DEFER-WH-01）。exists→put 非原子，并发成功上传允许重复通知。emitter 故障不改变上传结果。

## 来源适配：`lfs.media.finalized`（WH-06，已交付）

既有 media finalize 的 finalized manifest 实际 put 成功后发一次 `lfs.media.finalized`：scope 只填服务端 `MediaScope` 的 canonical `repo_path`（HTTP 路径带来的 `.git` 后缀会保留，例如 `/acme/app.git`；`lfs_paths` 必须写成同样形式，不能照抄 `git_paths` 的无后缀仓库路径），data 为 `oid,size,manifest_id,transfer="fastcdc"`。`occurred_at` 取本次 finalize 操作钟（生产 `finalize()` 在入口取 `unix_now()`，测试经 `finalize_at` 注入），与 reassembled 校验耗时同一快照。no-op / 中间步骤 / 中途失败不发；并发 finalize（同进程 `MAX_CONCURRENT_FINALIZE=2` 或跨进程）在 exists→put 窗口内可重复通知。HTTP 认证仍为 `AccessTokenUser` 的 DB access token（匿名/静态 push token 不发且 401）。见 [`fastcdc-media.md`](fastcdc-media.md)「出站事件」。

## 来源适配：`agent_capture.events.committed`（WH-07，已交付）

Agent Capture `events:batch` 在 ingest receipt 事务提交后，按本次实际 insert 的新 event 行**每 generation 一份** `agent_capture.events.committed`（ADR-WH-04；该 generation 新行数为 0 则不发）。摘要来自提交返回快照：`capture_id`、`receipt_id`、该组 `new_event_count` / `generation`、`stream_kind`（session 的 `session_kind`，不是水位流名 `jsonl`）、`completeness`，以及可信 `tenant_id` / `repo_path`。同批若插入两个 generation，就发两份摘要。HTTP 层同 fingerprint 的 replay 在进事务前 200 返回，不调用 commit、不发事件；事务内同 receipt 或仅旧 uid 的批 `new_event_count=0`，也不发。投影在 `try_emit` 同步完成，发送任务不回读 session/watermark 最新状态。过滤为 `agent_tenants` 与 `agent_repo_paths` 的 AND 交集。`push_auth=none` 不改变 ingest token 门。file-op / blob / session PUT 不发。见 [`agent-capture.md`](agent-capture.md)「出站事件」。

## 事件投影与过滤（WH-10）

`CommittedEvent` 只能表达六种冻结 `event_type`。投影结果为固定 envelope：`schema_version=1`、`event_id`、`event_type`、`occurred_at`、`source`、`scope{tenant_id,repo_path,oci_repository}`、`data`。超过 16 KiB 整事件丢弃，不截断。不含 actor、raw、object key、URL。

路径过滤：仅 `/` 可表示全树；其它过滤为 `p==f` 或 `p.starts_with(f + "/")`。非法 canonical path 丢弃。OCI 精确匹配；Agent 为 tenant 集合与 repo 集合的交集；LFS unscoped 仅 `include_unscoped_lfs=true`。

## 地址策略（WH-12）

每次 POST 只解析一次 DNS。解析结果必须全部为公共地址：loopback、RFC1918 私网、link-local、metadata（`169.254.169.254` / `fd00:ec2::254`）以及混合公共+受限结果一律拒绝。连接钉扎到本轮已验证的 IP，TLS SNI / hostname 仍使用原域名，发送时不再解析。生产路径没有把公共 IP 映射到本机的开关；测试用注入 resolver/pin。

`config/config.toml` 只保留注释块。`config/config-storage-only.toml` 写 `[storage_events] enabled = false` 占位，**禁止**提交可用 HMAC secret。

全部 `[storage_events]` 字段均为 restart-required：热更新只报告、不把候选值写入现有 Config 快照。

## 形态门

| 形态 | `enabled=true` |
|---|---|
| review（无 `git.push_auth`） | `config.validate` Err |
| storage-only `push_auth=token` | 接受（仍须 `installation_id`） |
| storage-only `push_auth=none` | 接受（仍须 `installation_id`） |

disabled 仍拒绝未知字段、重复 target id、非法 id 字符与空 `events`；跳过 SecretRef 解析与连通性检查。
