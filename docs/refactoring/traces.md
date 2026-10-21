# Libra Traces v1：Git 捕获归档与 Agent 历史检索契约

本文是 Mega2 与 Libra 对接的唯一协议事实源。状态为**已选定、待实现**；不表示下述路由、表、命令已上线。执行计划分别为 [Mega2 plan-20261015](../plan/plan-20261015.md) 与 [Libra plan-20261015](../../../libra/docs/development/plan/plan-20261015.md)。各仓任务卡、工程门与实际发布状态由对应计划维护；协议修订须同步两份计划并重新进行联合审查。

## 1. 产品边界与交接顺序

Libra 将工作期间捕获的脱敏记录写入 traces Git 历史，Mega2 接受 `refs/libra/traces` 作为独立版本化记录。Agent 通过只读接口检索相关历史、读取有界证据并核验来源，为当前工作提供上下文。上传只有 Git receive-pack；旧 `/api/v1/agent-capture` 及其数据按第 7 节退场。新 `/api/v1/traces` 为只读产品 API。

普通 `/project/foo.git` clone/fetch 不携带 traces 专属历史或对象。项目、身份、来源仓库与 stream epoch 分别确定隔离边界；traces 不成为代码默认分支，不进入 trunk/CL 合并，不出现在 Monorepo 代码目录。HTTP 与 SSH 两种 Git transport 都在首版范围。MegaUI、向量召回、模型生成摘要、事件/文件操作全量索引、隐藏 reasoning 解密不在首版范围。

`[traces].enabled` 在所有现有 profile 默认 `false`。`server_id` 是操作者持久配置的部署 UUID，启用时必填，进程不在启动时生成。配置重载和新进程重新初始化都读这份 UUID。证书轮换、凭据轮换、路径和启动次数都不会派生新的 `server_id`。`scope` 是可选操作者标签。协议预算不是配置字段。部署者在完成稳定 server/principal 绑定与权限前置后显式启用。H1/H3 的交接凭据须具名记录启用配置与服务 owner，不由计划提交自动修改线上开关。关闭时新 traces Git/API 面不挂载，旧 Capture 的现行开关独立，直到 H6 退场。

| 交接 | 生产方 | 消费方与先后约束 | 放行凭据 |
|---|---|---|---|
| H0 契约冻结 | 两仓 root 当前联合治理审查；Mega2 MTR-01 登记 | 两仓实现前置；LTR-00 已取消后续验收，不再作为生产者 | 两份计划及本契约联合 Claude 与隔离 Codex PASS；冻结 revision/hash 与两仓基线 |
| H1 Git 服务可用 | Mega2 MTR-12，前置 MTR-13 普通 clone 隔离 | Libra LTR-10/11/12 实现 scoped push，由 LTR-01 发布；LTR-02 clone 验收 | HTTP+SSH scope/auth/FF/对象隔离验收及发布凭据，或明确的受审本地双传输 fixture；不得以最后收口作为服务启动条件 |
| H2 scoped push 可用 | Libra LTR-01 | Mega2 MTR-14 起以真实 Libra 输入重建索引及验收 | 客户端发布凭据或受审本地 fixture；持久 source/epoch 和失败重试收据 |
| H3 查询服务可用 | Mega2 MTR-20 | Libra LTR-13/19 capability/readiness、LTR-24 target resolve、LTR-14/15/20 导航筛选、LTR-03/04/25/05 checkpoint/search/准入/read | 列表、关键词、预算读取与水位契约的实际验收；服务版本/fixture 明确 |
| H4 Agent 读客户端可用 | Libra LTR-03 / LTR-04 / LTR-05；隔离 live 用例由 LTR-21 / LTR-22 / LTR-23 提供 | Mega2 MTR-58 / MTR-60 / MTR-68 / MTR-69 / MTR-22 及 Libra LTR-07 互操作验收 | 三种 CLI 的机器输出与实际服务调用；另 pin 七条 scope/epoch/fresh-ODB live selector 的 Libra source revision，脚本消费前须可运行 |
| H5 互操作通过 | Mega2 MTR-22 / Libra LTR-07 | 旧 Capture 清理及两仓终态收口 | 两侧均消费 H1/H2/H3/H4 的同一 fixture/版本组合；双方 H5 不互相设为前置 |
| H6 旧 Capture 清除/退场完成 | Mega2 MTR-29 | Libra LTR-09、Mega2 MTR-30 | 旧对象（含历史版本）及 12 表清除收据、旧 surface 消失的发布证据 |

H1/H3 必须能够先于 H5 提供服务。后期联合验收不得反向阻塞早期服务或客户端，H6 也不能成为新 Git 接收/读 API 的启动前置。允许本地 fixture 作为开发依赖，不把本地可用称为远端已发布；正式完成仍须对应计划全部 C/D 门。

MTR-38 提供经授权的 discovery 元数据及 v0/v2 编码；真实 HTTP/SSH discovery入口接线由MTR-92承接，认证前置由MTR-11/MTR-39提供，H1 保留双传输实际验收。当前发布源码消费资格由 MTR-91 审计，不能据此补认取消卡的历史验收。

## 2. Scope、身份与 Git 虚拟入口

`stream` 的唯一自然键为 `(principal_id, target_id, source_repo_id, epoch_id)`，`hash_kind` 一经创建不可更改。四个 ID 都不是 commit 作者或 transcript 中的自报授权字段。

- `principal_id` 为服务端稳定 UUID。静态 push token 的具名凭据绑定与 Mono 账户绑定采用不同 credential namespace，由显式可信配置/账户映射关联该 UUID；轮换 token 保持绑定，删除/复用凭据必须显式处理。缺绑定不得写 traces，不能猜测用户名即不可变身份。既有普通代码认证不因本功能被迫新增绑定。
- `target_id` 为服务端稳定 UUID，登记目标类型（native/import）、当前正规化项目路径、可选 backing import repo ID 与 active 状态。路径是定位符而非永久身份；服务端显式移动事务更新项目及受影响后代路径。通用路径删除使 binding inactive，同路径重建不会继承旧 traces。禁止靠 tree OID 猜测重定位；管理者须显式 rebind。
- `source_repo_id` 由 Libra 已有 `libra.repoid` 提供。独立 clone 创建新的 ID，linked worktree 共用 ID；保留现有 opaque identity/legacy 值，不能静默改写为另一 UUID。服务端按经过编码的完整原值隔离；其作用是来源标签，客户端不能藉此获得别的 principal 的写权限。
- `epoch_id` 为客户端显式新建并持久化的 UUID。同一 stream 只追加；本地 clean 重写后必须用 `--new-stream` 开新 epoch，旧 stream 保留。创建 epoch 的重试复用已保存值，不每次随机生成。

正规化路径以单个前导 `/` 开头，去掉末尾 `/`（根路径保持 `/`），拒绝 `.`、`..` 和空路径段。路径中的空格保留，不把 `/project/foo ` 收成 `/project/foo`。`/app` 与 `/apple` 不是同一前缀。只有 Trunk 落地的目录提交才改 target。Review 形态只打开 change list，不改全局 target。首次 native 目录写入在 Trunk 目录提交成功之后，于同一个事务里再次确认路径仍是目录再插入。若插入后该目录已经不在当前树中，这次插入回滚。失败回滚会把这次记下的 target id 移回，包括期间被停用的行，不搬走后来出现在目标路径上的其它 target，然后再按当前树恢复 active。提交失败不插入，也不停用别人已经落地的绑定。唯一约束冲突表示路径已有绑定。同一路径的后续写入返回同一 UUID。显式目录 move 在目录提交前用同一个数据库事务更新该路径及其下已有 target 的 `project_path`，目录自身没有 target 时后代仍然跟着移动，UUID 不变；事务中任一更新失败则本次路径都不改。若更早的 push 在这次提交落地前把这些行停用，提交成功后只在该路径没有另一条 active target 时把这些 id 恢复为 active。目录提交失败则先按同样规则恢复，再把路径移回。停用同样是一个事务。目标路径已有另一条 active target，或目标落在自身子树内时，拒绝移动。显式目录 delete 在提交前记下当时的 active target id，只在提交成功后停用这些 id，且它们仍位于被删路径下。提交之后新建的 UUID 不会被这次删除停用。提交前失败不改 target。文件创建、移动和删除不新建 target。inactive 之后对同一路径的再次写入插入新 UUID，旧行保留。相同 tree OID 不改变 target 的 id 或路径。内部 rebind 只接受已经是 canonical 形式的路径；调用方未确认该路径存在，或传入带尾部 `/`、缺少前导 `/`、含 `.` / `..` 的字符串时不写库。公开 rebind 命令不在本卡。

通用代码 push 在 `b3_execute_push_inner` 提交前的同一事务中处理：没有 active target，或 landed tree 行不存在时，不改任何 target。push 路径自身保持 active。其下在 landed tree 中不再是目录的后代 target 设为 inactive；事务回滚后这些行恢复原状。ImportRepo 的 create、delete、move 仍返回 409，不插入也不停用 target。ImportRepo detach 在 `b3_execute_detach` 确认 live `repo_id` 仍等于 payload 之后、与删除 import ref、`git_repo` 和 cleanup ledger 的同一事务中，把 `project_path` 与 `backing_import_repo_id` 都匹配的 active target 设为 inactive，并把该 target 的 binding generation 从当前值加一后写入这次 detach 审计。没有匹配 target 时不写 generation。已 detached 或同路径已被新仓库替换的重放不改 target，也不再次增加 generation。同路径上的新 ImportRepo 使用新的 target UUID。旧 cleanup continuation 只清扫 ledger，不改同路径新仓库的 target。公开 rebind 命令是 `mega2 --config <明确现存配置> traces rebind-target --target <UUID> --path <canonical-path>`。它只接受已经是 canonical 形式、并且当前已经是目录的路径；目标 UUID 不存在、路径不是 canonical、或该目录不存在时不写库。配置必须由 `--config` 或 `MEGA_CONFIG` 具名给出，不生成默认配置，也不接受只在工作目录里发现的配置。

Git URL（所有动态 path segment 使用 UTF-8 bytes 的 base64url、无 padding；epoch 和 stream UUID 使用标准 UUID 文本）：

```text
/.traces/by-path/<encoded-target-path>/self/<encoded-source-id>/<epoch>/<sha1|sha256>.git
/.traces/by-scope/<stream-uuid>.git
```

`by-path` 用当前已授权路径解析 target，`self` 从本次认证凭据绑定 owner；纯 SSH 首次推送不需要先调用 HTTP 注册。不存在但合法的 scope 在 discovery 时只返回空 refs，不写 DB；首个成功 receive-pack 原子创建 target/stream。Git discovery 公布自定义能力 `libra-traces-v1`，scope 已存在时另公布 `libra-traces-scope=<stream-uuid>`（v0 receive-pack capability / v2 capability response 各自实现）。首次成功 push 的标准 `ok refs/libra/traces` 不增添 UUID 字段；Libra 再作一次只读 discovery 学习 UUID。若此读失败，保留按完整 by-path locator 绑定的成功 push 收据，标 `scope_resolution_pending`，下次 discovery 补齐，不能把已成功 push 改为失败或新建 epoch。通用 Git 可忽略未知能力。`by-scope` 以稳定 UUID 读取已经存在的 stream；只有 owner 能更新。两种入口均只暴露 `refs/libra/traces`，不公布代码 refs，不伪造 `HEAD -> refs/heads/main`。

Repo locator 必须先判定 Traces，再进入 Native/ImportRepo。TraceRepo 不创建 `git_repo`、`.gitkeep`，不进入 MonoWriteQueue/PushChain/CL。SSH `traces@host` 的 none-auth 必须拒绝，再以实际 key/token 建立 principal；普通匿名代码读取维持已有语义，但任意匿名会话请求 traces 必须拒绝。storage-only 的普通 SSH receive-pack 限制不因此放开，只有独立 TraceRepo 例外。

已发布的 MTR-38 基线：`dispatch_repo_locator` 先解析上面两条语法，再判定 view 或 Native/Import 路径。契约 URL 必须以单个前导 `/` 开始并以 `.git` 结尾；缺少后缀、多余前导 `/`、相对路径或经 `.`/`..` 折叠才到 `.traces` 的别名均返回 `traces locator not found`，不落入 Native/ImportRepo。HTTP/SSH 进入 handler 前会去掉这一个后缀，`parse_traces_locator` 仍识别去掉之后的路径。`.traces` 命名空间里语法不合法时同样返回 `traces locator not found`。`classify_repo_locator` 对合法 traces URL 返回 `traces locator is not a code repository`，不返回代码 Path，因此现有代码 LFS 重写不会把该 URL 当成 Native/ImportRepo；SSH 的 traces LFS 命令也被拒绝。UUID 只接受小写标准文本。`parse_git_protocol_path` 把识别结果放在 `GitProtocolPath.traces`，并把仓库路径交给 `SmartSession`。`SmartSession::from_state` 对 by-path 使用 URL 里的 `sha1` 或 `sha256` 作为这次会话的 object format，discovery 的全零 id 宽度与广告的 `object-format` 一致。by-scope 仍用服务器配置的 hash kind。`repo_handler_with_commands` 在 ImportRepo 查找/注册和 Monorepo 构造之前返回 `TraceRepo`。SSH `parse_ssh_exec_request` 同样先得到 traces locator，合法 URL 不按 view 拒绝。合法 locator 的 discovery 返回空 ref 列表和全零 id，协议广告使用 `capabilities^{}`，不伪造 `HEAD -> refs/heads/main`，也不写 DB；已有 stream 的 tip 广告不在本卡。TraceRepo 在 pack 解码前拒绝代码 pack，保存、finalize、ref 更新和树遍历也都拒绝，不创建 `git_repo` 或名为 `.gitkeep` 的 blob，也不调用 MonoWriteQueue、PushChain 或 CL。专属 discovery 元数据和 v0/v2 编码器已实现，但真实 HTTP/SSH 入口接线仍待 MTR-92；MTR-11/MTR-39 的认证及传输入口职责保持。

MTR-94 的源码实现在 HTTP Router 外层增加禁用守卫：`[traces].enabled=false` 时，GET/POST 请求一旦进入根 `.traces` 保留命名空间，在业务鉴权、请求解压、读取请求体及 Native/Import 分派之前返回 HTTP 404，JSON 错误响应的消息为 `traces locator not found`。守卫只读取配置，不查询 scope 存在性，不进入代码仓 fallback，也不启用 traces 数据面。LFS 路由重写前，服务端以请求扩展保留原始 URI；守卫同时检查它与当前路由 URI，不接受客户端 header 作为原始路径依据。对每个路径均检查原始字节与恰好一次百分号解码后的字节路径，因此 GET/POST 请求中已重写为 `/info/lfs/…` 的保留别名也不会进入 LFS handler；重复 `/` 和 `.` 不增加目录深度，`..` 回退一层，进入根 `.traces` 后即拒绝，即使后续 `..` 又离开该根。对于 `/info/refs`、`/git-upload-pack`、`/git-receive-pack` 后缀，另检查只去掉仓库名的一个末尾 `.git` 的结果，因此 `/.traces.git/info/refs` 也拒绝。此处仅说明禁用守卫的匹配范围，不把这些别名变成合法 locator。

本规则按请求 URL 判断：即使已有代码仓根名在数据库中字面为 `%2etraces`，其 `/%2etraces/…` URL 在禁用时仍按保留别名拒绝；启用时本禁用守卫保持该字面根的既有分派，不表示 enabled traces 数据面已经可用。单次编码的 `/%2etraces/…`、`/.traces%2F…` 会命中；双重编码的 `/%252etraces/…` 不会在此守卫中再次解码。大小写不同的 `/.TRACES/…`、近似名称 `/.traces-backup/…`、普通嵌套 `/project/.traces/…` 以及 `/.traces.git/ordinary.git/info/refs` 不因该守卫而被当作保留根，继续原有分派；这不承诺其后续请求成功或符合 traces locator 语法。上述描述源码行为；本次实现的验收、审查与发布状态以[执行计划](../plan/plan-20261015.md)为准，不由本契约声明完成。SSH 对应禁用入口仍待 MTR-100；enabled 数据面的 HTTP/SSH 认证与传输入口仍待 MTR-11/MTR-39，专属 discovery 接线仍待 MTR-92。DEBT-MTR-01 保持 open，须 MTR-94/MTR-100 各自真实验收及完整 C/D 后才关闭；MTR-38 旧发布收据不覆盖本次改动。

项目已有读取权限即可读取该项目全部 active 可访问 traces；写入同时要求当前目标写权限和 stream owner 身份（首次创建 owner 绑定当前 principal）。所有列表、搜索候选、计数、snippet、原文和 Git wants 必须先限定到授权 scope；未知及无权限资源均 404，缺认证为 401。资源读权限每次重查，cursor 不充当授权凭证。

权限来源以当前 Mega2 实现为准：静态 token 对 target 当前正规化路径使用 `token_covers_repo`/配置 `paths` 范围门；Mono 账户消费现行 `viewRepo` / `pushRepo`、root Repository resource 与已验证 entity snapshot。当前没有独立的 target ACL 表，本计划不假定每个 target 已有一套独立 ACL。新 traces auth helper 统一承担身份绑定、路径范围和 read/write 动作校验，HTTP/SSH/查询均调用它；现有 code anonymous 门、只用于 CL 的 `cedar_guard` 和 `UnprotectedRequest` 不能作为 traces 权限放行依据。可信身份/snapshot/动作判定缺失时 fail-closed。

当前 helper 是 `ceres::traces::auth::authorize_traces`。静态 push token 的凭据 id 是 token 名称，不是 secret；只匹配 `static_push_token` 命名空间中未退役的绑定，再对 canonical target path 调用 `token_covers_repo`。Mono 账户只匹配 `mono_account` 命名空间，读评估 `viewRepo`，写评估 `pushRepo`，资源是 root Repository 快照。快照缺失、`enforcement` 不消费快照，或 Cedar 拒绝时，traces 拒绝；`off` / `shadow` 对代码推送的放行不放行 traces。匿名调用得到未认证结果。没有绑定、不是 owner，或缺少目标读/写权限时得到不可区分的拒绝。提交作者不参与判定。普通代码读取仍走既有匿名读门，普通代码写入仍走既有 token 路径门和 `decide_push`；没有 traces 绑定不改变这些代码结果。HTTP、SSH 与查询路由接入该 helper 由后续卡完成。

## 3. 原始对象存储、接收与恢复

原始 Git payload 是证据事实源。commit/tree/blob/tag 均保留原始 bytes，OID 根据 Git kind/length/payload 重新校验；不能用现有 commit/tree ORM 模型重序列化替代原始 bytes。SHA-1/SHA-256 的协商、长度与包头必须一致，禁止跨 hash domain fallback。

```text
traces/v1/<stream-uuid>/objects/<sha1|sha256>/<OID shard>
traces/v1/staging/<ingest-uuid>/...
```

`traces` 是独立对象 namespace（稳定字符串 `traces`），不与 `git` 或 `agent` 共用。该 namespace 下的逻辑 key 保持完整，不按对象 key 的前 6 字节再切片，所以上面的布局就是后端路径 `traces/` 之后的内容。OID shard 由服务端从 OID 文本派生：短于 6 个 ASCII 字节时 OID 本身作为最后一段；更长时为 `aa/bb/cc/<剩余十六进制>`，恰好 6 字节时写成 `aa/bb/cc`，不产生空路径段。stream id 使用小写标准 UUID，hash kind 只接受 `sha1` 或 `sha256`，OID 只接受十六进制并按小写归档。相同字节再次写入同一 stream、hash kind 与 OID 时保持已有 payload；不同字节被拒绝。同一进程内的并发写入按这个规则串行，后到的冲突不会覆盖先写入的字节。本归档不插入 `trace_object` 行，也不要求 stream 行已经存在。

对象 key 由服务端派生，客户端不能指定 object key。对象字节可以在同一 stream 内去重；查询或 delta base 不允许从全局 `git/` 或别的 stream 回退查找。字节已落存储不等于对象已对客户端可见；只有已发布 membership 且从本 stream ref 可达的对象才可成为读/pack/want/delta 的依据。

接收顺序固定：认证和目标授权 → 有界 quarantine 解包 → 校验 OID/闭包/单父 FF 与 checkpoint 不可变 → 所有新对象 durable → 单 DB 事务重新核对 old tip、principal/凭据有效性、目标 active/binding generation 和当前项目写权限，再创建必要 scope、发布 membership、CAS 更新 ref、写 ref journal/outbox → 返回 Git `ok` → 后台索引。权限/目标变更与发布使用已有权威写入屏障或版本 CAS，不可在解包时的旧授权上无条件发布。第一次创建 stream 也遵守此事务顺序；并发首次 push 由自然键唯一约束和 tip CAS 确定唯一赢家。

已落地的引用发布只接受唯一 ref `refs/libra/traces`。删除、非快进、父数大于 1 的新提交、改掉已有 checkpoint 的 `original_tree`，或让已有 checkpoint 从本次提交中消失，都拒绝，并且不改 tip、membership 或 journal。通过校验后，同一数据库事务把本次对象 membership 标为可见、用当前 tip 做 CAS 更新 `trace_stream.tip_oid`，并追加一条 `trace_ref_update`。同一 old tip 的并发发布只有一个事务成功。同一 idempotency key 在 tip 已经是该 new tip 时返回原 journal，不再写第二条。对象字节在该事务开始前写入 `traces` namespace；事务失败时这些字节仍不可见。已知 schema 标识是 `manifest-v1` 与 `metadata-v2`。其它 schema 仍按上述规则归档，发布结果标为 `unsupported`，不拒绝这次引用更新。同一事务在写入 membership、移动 tip 或追加 journal 之前重查 credential 仍未退役、target 仍在本次授权路径上处于 active、binding generation 与发布前一致，以及调用者对这条当前路径仍有写权限。解包期间 credential 退役，或 target 改到别的路径，都会拒绝这次发布，并且不移动 tip、membership 或 journal。没有出示 credential 时这次拒绝是 `unauthenticated`；其余重查失败是 `unknown_scope`，不收成 `invalid_parameter`。Mono 账户的写权限在这次事务里读取进程当前的 Cedar 快照；解包前留下的快照不能放行。快照缺失或该快照被标记为 dirty 时拒绝。`off` 不消费快照，因此拒绝。`shadow` 仍按这份当前快照的 Cedar 决定，不会把代码推送里的 shadow 放行当成 traces 写权限。

已落地的解包先把 loose object 写入 `traces/v1/staging/<ingest-uuid>/objects/...`。这个前缀不是已发布 object key，也不读取其它 stream 或 `git` namespace 里的基对象。`ref-delta` 与 `ofs-delta` 直接拒绝。每个 blob、tree、commit、tag 的声明 OID 按 Git `type size\\0payload` 用 sha1 或 sha256 重算，不符则拒绝并且不把该对象写入已发布 key。commit、tag、tree 引用的对象必须已经在本次 quarantine 或本 stream 已归档字节中，否则在 ref 发布前拒绝。即将成为可见 membership 的每个 OID，包括 new tip，也必须已经在本 stream 有字节，否则不发布 ref。压缩字节数超过 512 MiB，或本次累计解包字节数超过 1 GiB，立即停止，超限的那个对象不写入 quarantine。staging 恢复只删除尚未出现在可见 `trace_object` membership 中的暂存对象；可见 membership 和已发布 object key 在崩溃恢复时保留。同一次解包还拒绝超过 100,000 个对象、10,000 个新 commit、64 层 delta 或 tree、300 秒接收时限，以及单个对象超过 64 MiB 的尝试。这些上限读取 `TracesProtocolLimits`。超限或取消本次 worker 时不写入该对象，也不发布 ref；取消会清掉这次 worker 上的计数。闭包通过之后、ref 发布之前，本次每个新 commit 里 `checkpoint/<prefix>/<rest>/` 下的 `metadata.json` 与 `manifest.json` 还要按原始 blob 字节对照 `MAX_METADATA_BYTES` 与 `MAX_MANIFEST_BYTES`。其它路径上的同名文件不走这道门。超限不发布 ref。

每次 push 只允许更新唯一 `refs/libra/traces`；拒绝额外 ref、删除、非 FF、force rewrite 与 merge commit。对所有新提交逐一校验：已有 checkpoint ID 的 subtree/tree OID 不变且不删除，新增内容必须使用新 ID；不能只比较最终 tip 从而漏掉中途篡改。

v1 固定每个对象最大 64 MiB，metadata/manifest 各最大 1 MiB，压缩 pack 最大 512 MiB、解包总量最大 1 GiB；每 push 最多 100,000 个对象、10,000 个新 commit，delta/tree 最大深度各 64，接收总时限 300s（固定协议值与广告字段见第 5 节，取消时释放资源）。v1 不提供这些预算的 operator override；capability 如实公开协议常量。流式处理在解码/遍历期间累计 bytes、计数、深度和时限，超限立即停，不先整体分配 pack 大小。断开后未发布 staging 由 lease+membership 校验清理，GC 不扫描/删除已发布对象或其他 scope。对象 durable 后 DB 失败只留下不可见 orphan；DB 成功但回复丢失的重试检查同 tip 并返回幂等结果，不重复 journal。

## 4. 读模型与关键词索引

身份基础表为 `trace_principal`、`trace_credential_binding`；对象与引用基础表为 `trace_target`、`trace_stream`、`trace_ingest_attempt`、`trace_object`、`trace_ref_update`。其中 `trace_principal`、`trace_credential_binding` 与 `trace_target` 由迁移 `m20261015_000100_trace_identity` 创建，`down` 为 no-op。`trace_stream`、`trace_ingest_attempt`、`trace_object` 与 `trace_ref_update` 由迁移 `m20261015_000200_trace_publication` 创建，`down` 为 no-op。stream 自然键是 `(principal_id, target_id, source_repo_id, epoch_id)`，`hash_kind` 创建后不可改。派生读模型为 `trace_checkpoint`、`trace_session`、`trace_resource`、`trace_code_link` 与 `trace_fragment`。这五张表和 `trace_fragment_membership` 由迁移 `m20261015_000300_trace_projection` 创建，`down` 为 no-op。checkpoint 键在 stream 内唯一，`original_tree` 创建后不可改。resource 持久化 `ordinal` 与逻辑 `byte_start`/`byte_end`。fragment 以 `(stream_id, content_digest, index_version)` 唯一，并保存 `ascii_fold` 文本列；membership 记录 checkpoint 与 resource 出处，这些出处列创建后不可改。resource、code link 与 membership 用复合外键保持 checkpoint、resource 和 fragment 属于同一 stream。该迁移不 CREATE/DROP EXTENSION，也不建 trigram GIN。旧 Capture 维护另有 `trace_legacy_writer_fence`、`trace_legacy_purge`、`trace_legacy_purge_item`，只服务第 7 节，不属于 Agent 读模型，也不随旧 12 表删除；其显式 DROP migrator 使用独立 `traces_maintenance_schema_migrations` ledger。表的外键/唯一约束以 stream 为边界；每个 checkpoint ID 在 stream 内唯一且不可变。`trace_ref_update` 同时提供持久索引任务，具备 claim lease、幂等处理、重试/backoff、失败状态和恢复入口；不能依赖仅内存通知。

后台 worker 从经过验证的原始 manifest 重组有序 transcript parts；支持当前 manifest v1、metadata v2 和明确登记的 lifecycle schema。校验每 part path/OID/type/byte_len、总长度和 content hash。未知 schema 保存归档并标 `unsupported`；已知 schema 语义损坏标 `invalid`。原始 Git 闭包/不变性违反仍在 push 前拒绝。索引只推进到已经完整处理的连续 ref sequence，重启可重放，失败不得跳过后记全部 indexed。

`Libra-Parent-Commit` 只形成 `trace_code_link`，不是 traces Git parent，也不是授权依据。code OID 缺失允许 `unresolved`，仅在本目标代码 scope 中解析；不能为解决 code link 读取别的项目。metadata 中 `modified_files` 等提取是最佳努力；不能把本地绝对路径直接映射为服务端项目文件，不能宣称完整文件操作或完整 turn/subagent 图已同步。

关键词检索针对**脱敏 transcript 原始 UTF-8 文本**，首版为字面关键词/短语，不解析统一消息模型。`q` 为 1..256 UTF-8 bytes，ASCII 字母大小写不敏感，非 ASCII 按原字符字面匹配；不做 Unicode normalization、stemming、regex、模型推理或语义近邻。SQL `%`、`_`、反斜杠必须按字面转义，查询使用绑定参数。

`trace_fragment` 以 stream 内资源 content digest + `index_version` 去重，并保留 checkpoint/resource membership。16 KiB UTF-8 边界分片，相邻片段至少 256 bytes 有效重叠；分片按 manifest 的逻辑重组流构建，跨 Git part/UTF-8/JSONL 长行的命中不遗漏。存储原文及 ASCII-fold 搜索列，使用 PostgreSQL `pg_trgm` GIN 做候选索引，应用层按同一匹配定义复核并映射原文 bytes。`pg_trgm` 由具名 operator／测试栈在临时测试 schema 之外的固定可信 `mega2_extensions` schema 预置（operator 拥有，namespace／版本纳入服务交接清单），期望扩展版本固定为 `pg_trgm 1.6`，初始化 SQL 显式指定 `VERSION '1.6'`，测试／operator receipt 仅核验实际版本与 namespace，不把自报 receipt 当作运行时期望版本。[PostgreSQL 18 官方扩展定义](https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/contrib/pg_trgm/pg_trgm.control) 的默认版本为 1.6。运行时从受审规范对应的 canonical 常量比较实际 catalog；普通应用迁移只创建 Traces 表及不依赖扩展的约束／索引，不在每例 schema 创建或删除扩展，也不把 GIN 作为这些迁移成功的前置。扩展缺失、版本／namespace 不符或缺建索引权限时，代码服务与关闭的 Traces profile 仍正常启动；已开启的 Traces Git 归档及元数据投影继续工作，搜索部署 readiness 返回 `search_ready=false` 和稳定原因。测试栈／CI／隔离测试数据库设施由 MTR-70 承接：测试 compose 挂载 `docker/traces_pg_trgm_init.sql`，schema owner 为 NOLOGIN 角色 `mega2_traces_ext_owner`，无扩展用例经 `MEGA2_IT_DB_ADMIN_URL` 从 `template0` 创建唯一 `traces_noext_<pid>_<counter>` 数据库。随仓库部署 bootstrap、既有 volume／独立 views DB 的显式 operator 操作及具名 app 角色对可信扩展 schema 的 USAGE／扩展函数 EXECUTE 授权由 MTR-72 承接（app 不成为 schema owner）：storage-only compose 挂载同一份 `docker/traces_pg_trgm_init.sql`，operator 入口是 `scripts/traces_pg_trgm_bootstrap.sh`。MTR-74 是 canonical GIN descriptor／DDL 与物理 catalog 校验的唯一实现（index_version=1 对应固定索引名 `trace_fragment_ascii_fold_trgm_v1` 的 `_v1` 后缀，并核验完整 DDL 定义）；method、opclass、columns、index_version、indisvalid、indisready 以及扩展版本、可信 schema owner 各自校验。operator 补齐后，MTR-71 的受控 builder 消费这些结果，完整合格后才发布 ready；失败或未完成不能假 ready。生产超时／索引缺失／确切自有 invalid 索引的恢复由 MTR-73 正式维护 CLI 承接，复用同一 descriptor／DDL 和排他租约；执行预算默认 1800s，operator 可在 60..7200s 内调整，锁等待预算单独为 30s。仅对当前 catalog 与受审身份匹配的自有索引执行受控修复并重读新 catalog；未知或不匹配索引拒绝变更，后台不自动删除。 CLI 只核验并报告 catalog；IndexState 由每个服务进程自行推导。MTR-71 的存活后台固定每 60s 重查，外部维护产生完整 Valid 后无需重启转为 Ready；仍 invalid／不匹配时保持 Unavailable。RepairIdentity 的构造、授权及处置唯一归 MTR-73，MTR-74 的 Valid 仅决定能否 Ready，不作为修复 invalid 的先决条件。资源处理水位不代表 GIN 可用。查询／GIN operator class 显式使用已核验的可信 namespace；schema guard 仅清理本例 schema，不能回收栈级扩展或其成员对象。[PostgreSQL 扩展定义](https://www.postgresql.org/docs/current/sql-createextension.html)明确扩展名称在数据库内唯一，成员对象可归属指定 schema。缺失或版本／namespace 不符时索引能力显式不可用，不能降级成请求时无界下载原始历史。readiness 还须验证当前数据库 locale/ctype 对 CJK 的可提取 trigram 及 EXPLAIN 索引路径，返回 `cjk_index_supported`；无法索引的 CJK 查询与短词、纯标点一样改走有预算的 keyset 扫描并明确 `search_backend=bounded_scan`，不以完整索引宣称覆盖。[PostgreSQL 官方契约](https://www.postgresql.org/docs/current/pgtrgm.html) 说明其支持 LIKE 索引，缺少可提取 trigram 的 pattern 会退化扫描，因此不能把这种 pattern 直接送入无界 GIN/full scan。

首请求同时固定 target 内每 stream 的 `accepted_seq` 与 `indexed_seq`，候选严格限定为不大于**固定 indexed_seq** 的资源；index 追赶和后续新 push 均不加入已有 cursor，重新发起查询才能消费新增索引。cursor 绑定 query、filters、scope、两组水位和 index_version，ACL 每页重核；cursor 过期/参数变化返回稳定错误。结果排序固定为 capture 时间降序，再 stream/checkpoint ID/逻辑 byte start，属于可重现排序，不称为语义相关度。完全相同片段在同一 stream 内合并展示；每条最多返回一个固定代表证据及 `occurrence_count`，不能展开无界 checkpoint 引用。不同资源/来源身份仍可单独通过有界列表读取，不跨 principal 合并证据身份。

## 5. Agent 只读 API 与 Libra 消费

所有路径以下列 `/api/v1/traces` 为前缀，均为 GET：

| 相对路径 | 输入/用途 |
|---|---|
| `/capabilities` | 已认证返回 protocol=`libra-traces/v1`、稳定部署 UUID `server_id`、Git transport/object-format 支持、API/schema/index version、固定协议预算和 search readiness |
| `/targets/resolve` | `path` 定位已登记且可读 target；不创建 scope |
| `/targets/{target_id}/streams` | source/owner/epoch 筛选，返回 accepted/indexed tip 和状态 |
| `/streams/{stream_id}/sessions` | agent/time/code OID/session 筛选，列表不带正文 |
| `/streams/{stream_id}/sessions/{session_id}` | session 元数据、捕获与提取完整性 |
| `/streams/{stream_id}/sessions/{session_id}/checkpoints` | checkpoint 列表、schema/索引状态和资源大小 |
| `/streams/{stream_id}/checkpoints/{checkpoint_id}` | 原始 metadata/manifest 的经过验证投影、资源定位和 code link |
| `/streams/{stream_id}/checkpoints/{checkpoint_id}/transcript` | `cursor`、`max_bytes` 有界读取逻辑 transcript，可起始指定字节 offset；不读取 reasoning 密文正文 |
| `/search` | 必填 `target_id`、`q`，可选 source/stream/session/agent/time/code OID；返回原文命中及来源 |

`/capabilities` 的搜索能力字段固定为 `search_ready: boolean`、`cjk_index_supported: boolean`、`bounded_scan_supported: boolean`。`search_ready=true` 表示当前部署的查询服务、所需扩展及索引版本通过 readiness 检查，能够按预算执行查询；false 时同时返回稳定 `search_unavailable_reason` 字符串。`cjk_index_supported=false` 不使搜索整体不可用，CJK 查询走 `bounded_scan_supported=true` 的预算扫描。它们不替代每个 stream 的索引水位，也不代表全部历史已有索引。

`/capabilities` 的 `limits` 对象逐项公开下表 22 个 `u64` 字段。v1 的 22 项均为固定协议常量；广告与实际准入、请求默认及响应预算引用同一常量来源，不提供 operator override，TOML 中未声明的预算字段／namespace 按现有未知字段规则拒绝。请求仍可显式选择 `limit`／`max_bytes`，必须在固定边界内；省略时使用固定协议默认值。修改常量须先修订此契约、两份计划及兼容性门。KiB/MiB 均按 1024 进制。当前代码只声明 `TracesProtocolLimits::MAX_OBJECT_BYTES`、`MAX_METADATA_BYTES`、`MAX_MANIFEST_BYTES`、`MAX_PACK_BYTES`、`MAX_DECODED_BYTES`、`MAX_OBJECTS`、`MAX_NEW_COMMITS`、`MAX_DELTA_DEPTH`、`MAX_TREE_DEPTH`、`INGEST_TIMEOUT_SECONDS`、`METADATA_LIMIT_DEFAULT`、`SEARCH_LIMIT_DEFAULT`、`METADATA_LIMIT_MAX`、`SEARCH_LIMIT_MAX`、`TEXT_BYTES_DEFAULT`、`TEXT_BYTES_MAX`、`QUERY_BYTES_MIN`、`QUERY_BYTES_MAX`、`QUERY_TIMEOUT_MS`、`RESPONSE_BYTES_MAX`、`CANDIDATE_FRAGMENTS_MAX` 与 `SCAN_BYTES_MAX`。当前代码已声明表中全部 22 个常量。本段不提前定义表外行为。

| limits 字段 | 固定值 | 唯一常量来源 | 预算性质 |
|---|---|---|---|
| `max_object_bytes` | 67108864 | `TracesProtocolLimits::MAX_OBJECT_BYTES` | 固定协议值，不支持 operator override |
| `max_metadata_bytes` | 1048576 | `TracesProtocolLimits::MAX_METADATA_BYTES` | 固定协议值，不支持 operator override |
| `max_manifest_bytes` | 1048576 | `TracesProtocolLimits::MAX_MANIFEST_BYTES` | 固定协议值，不支持 operator override |
| `max_pack_bytes` | 536870912 | `TracesProtocolLimits::MAX_PACK_BYTES` | 固定协议值，不支持 operator override |
| `max_decoded_bytes` | 1073741824 | `TracesProtocolLimits::MAX_DECODED_BYTES` | 固定协议值，不支持 operator override |
| `max_objects` | 100000 | `TracesProtocolLimits::MAX_OBJECTS` | 固定协议值，不支持 operator override |
| `max_new_commits` | 10000 | `TracesProtocolLimits::MAX_NEW_COMMITS` | 固定协议值，不支持 operator override |
| `max_delta_depth` | 64 | `TracesProtocolLimits::MAX_DELTA_DEPTH` | 固定协议值，不支持 operator override |
| `max_tree_depth` | 64 | `TracesProtocolLimits::MAX_TREE_DEPTH` | 固定协议值，不支持 operator override |
| `ingest_timeout_seconds` | 300 | `TracesProtocolLimits::INGEST_TIMEOUT_SECONDS` | 固定协议值，不支持 operator override |
| `metadata_limit_default` | 50 | `TracesProtocolLimits::METADATA_LIMIT_DEFAULT` | 固定协议值，不支持 operator override |
| `search_limit_default` | 20 | `TracesProtocolLimits::SEARCH_LIMIT_DEFAULT` | 固定协议值，不支持 operator override |
| `text_bytes_default` | 16384 | `TracesProtocolLimits::TEXT_BYTES_DEFAULT` | 固定协议值，不支持 operator override |
| `metadata_limit_max` | 200 | `TracesProtocolLimits::METADATA_LIMIT_MAX` | 固定协议值，不支持 operator override |
| `search_limit_max` | 100 | `TracesProtocolLimits::SEARCH_LIMIT_MAX` | 固定协议值，不支持 operator override |
| `text_bytes_max` | 131072 | `TracesProtocolLimits::TEXT_BYTES_MAX` | 固定协议值，不支持 operator override |
| `query_bytes_min` | 1 | `TracesProtocolLimits::QUERY_BYTES_MIN` | 固定协议值，不支持 operator override |
| `query_bytes_max` | 256 | `TracesProtocolLimits::QUERY_BYTES_MAX` | 固定协议值，不支持 operator override |
| `response_bytes_max` | 1048576 | `TracesProtocolLimits::RESPONSE_BYTES_MAX` | 固定协议值，不支持 operator override |
| `candidate_fragments_max` | 2000 | `TracesProtocolLimits::CANDIDATE_FRAGMENTS_MAX` | 固定协议值，不支持 operator override |
| `scan_bytes_max` | 8388608 | `TracesProtocolLimits::SCAN_BYTES_MAX` | 固定协议值，不支持 operator override |
| `query_timeout_ms` | 2000 | `TracesProtocolLimits::QUERY_TIMEOUT_MS` | 固定协议值，不支持 operator override |

筛选的 wire query 名固定为 `source`（完整 source_repo_id 字面值）、`owner`（principal UUID）、`epoch`（epoch UUID）、`stream`（stream UUID）、`session`（完整 session_id 字面值）、`agent`（metadata.agent_kind 原值，例如 `claude_code`，不猜测另一套 provider slug）、`code_commit`（完整 40/64 位十六进制 code OID）、`since`、`until`；各路由仅接受表中列出的适用项。`since/until` 接受 RFC3339 带时区时间并正规化到 UTC，区间为 `[since, until)`，未指定边界即无该边界；since 晚于 until 返回 400，等值为空区间。`capture_time` 来自已知 metadata 的 `created_at` Unix **秒**（现行 writer 使用 `timestamp()`；history.rs 的验证明确拒绝误传毫秒值），不使用 Git author 时间或服务器接收时间补造；无效值标 invalid。sessions 按满足筛选的 checkpoint 关联入选，并按其中最大 capture_time 排序；search 按其证据 checkpoint 的 capture_time 筛选。客户端网络前检查与服务端检查采用相同定义，未命中不自动扩大区间。

列表响应 `{schema_version, items, next_cursor}`，固定协议默认 50、最多 200。search 固定协议默认 20、最多 100 条命中；所有搜索/读取文本预算固定协议默认 16 KiB、最大 128 KiB UTF-8 bytes，整个 JSON 响应最大 1 MiB（包括 metadata/引用），超长单条 JSONL 可返回明确标记的 `partial_record`，不能突破预算。预算是 bytes，不承诺未指定 tokenizer 的 token 数。search 单次至多扫描 8 MiB 原文候选、2000 个片段；DB statement timeout 2s。预算耗尽返回实际结果、`partial=true`、继续 cursor；DB timeout 返回 503 `query_timeout`，不伪装成无命中。

片段响应包含 `stream_id`、`session_id`、`checkpoint_id`、`checkpoint_tree_oid`、`traces_commit_oid`、`manifest_oid`、资源 logical path/byte range，以及必要的 part OID/range；正文、是否截断与 `next_cursor` 同时返回。片段身份完全由已授权 checkpoint 的 manifest 派生，禁止接受任意对象 OID作为读取权限。

查询响应额外带 scope 内 accepted/indexed watermarks、`index_state`、`unsupported/invalid` 数量、`coverage_complete`。仅当快照中全部相关资源已索引且本次查询扫描完整时才能标 complete；没有匹配但 index_pending/partial 仍是未完全搜索。原始记录无任务摘要、成功结论或决策理由的统一字段，接口不伪造这些事实。

缺认证 401；无权限/未知 scope 404；非法参数/cursor 400；读取已归档但不可解析 schema 409 `unsupported_schema`/`invalid_capture`；完整性损坏 409 `object_integrity`；单个投影连同 envelope 无法装入响应上限时 413 `response_budget`（返回完整错误 JSON，不能截断 JSON bytes）；部署 `search_ready=false` 时 `/search` 返回 503 `search_unavailable` 与同一稳定 reason。单 stream 的 index_pending 在服务可用时返回 200 并如实给出水位/coverage，不混同部署搜索不可用；临时索引未就绪在 metadata 正文中体现，不把 redacted-only 当 `missing_raw`。错误 DTO 不带内部 key、凭据、绝对工作目录或正文。运行时 OpenAPI 是 schema 证据，所有路径与状态注册在同一类型契约中。

当前 `TracesError` 登记这些稳定码：`unauthenticated` 401、`unknown_scope` 404、`invalid_parameter` 400、`invalid_cursor` 400、`unsupported_schema` 409、`invalid_capture` 409、`object_integrity` 409、`response_budget` 413、`query_timeout` 503、`search_unavailable` 503、`trace_history_in_code` 409。HTTP `ApiError` 与 Git `ProtocolError::Traces` 读取同一张映射，Git 侧保留该分类，不把这些码收成普通 invalid input。公开文本只有稳定码；`search_unavailable` 另带经过校验的 reason token。对象 key、凭据、绝对路径和正文不会进入错误文本。

Libra 提供：

```text
libra agent push --mega2 --remote <name> [--new-stream]
libra mega2 traces capabilities|targets|streams|sessions|show|checkpoints|search|transcript
libra mega2 traces fetch --remote <name> --stream <uuid>
```

所有读取命令支持已有 `--json`/`--machine` envelope；正文必须在结果字段内，不把原始 transcript bytes 混入 JSON stdout。`search --target /project/foo --query "connection timeout"` 先解析 target；`transcript --stream <id> --checkpoint <id> --max-bytes 16384` 只读取指定证据。延续已有 token-file/auth 解析，日志和错误不输出 secret。能力检测失败清楚报错，不猜测 URL 就启用 Mega2 特殊行为；pure SSH push 使用 Git discovery，不强依赖 HTTP capability 注册。

`capabilities --require-search` 只把既有 search readiness 作为本次命令成功前置，不增加服务端 schema；未指定时正常报告能力状态。它不等同于所有历史已完成索引，具体查询仍须核验自己的 accepted/indexed 水位与 coverage。

Mega2 scoped SSH 连接固定使用保留 login `traces`（`traces@host`），仅派生本次 locator，不改写配置中的普通代码 remote。服务端以实际 key/token 解析 principal；这个保留 login 不能被当成 Mono 账户名授权。SSH 主机/密钥选择仍遵守现有显式配置，live fixture 必须实测 key/token 认证而不能依赖匿名会话成功。

客户端 source/remote/target/epoch 绑定保存到**仓库级**现有配置，以本仓现有操作 lease 保护并发更新，linked worktree 同源共用；不新增 global/system 配置 migration。首次 epoch 在网络尝试前持久化，并保存 remote/target scope，服务端返回的稳定 stream UUID在成功后绑定。已知 UUID 后续推送使用 by-scope 入口，目标移动仍按稳定 target 及其当前权限判定；UUID 尚未解析时保留 by-path 收据，路径失效须显式处理，不猜测另一个新项目。`--new-stream` 和同 epoch push 串行，失败不静默更换 epoch。Mega2 模式拒绝 `--force-rewrite`。

显式 traces fetch 写 `refs/remotes/<remote>/traces/<stream-uuid>/<hash-kind>`，同时保存经过认证的 canonical server fingerprint/stream/hashkind 收据。fingerprint 为 SHA-256(`libra-traces/server/v1\0` + server_id + 正规化 transport authority)，authority 包括 scheme/host/port/service root，遵守原有 TLS/SSH 主机认证，证书正常轮换不改变它；Git discovery 同时提供 `libra-traces-server=<server_id>` 供纯 SSH 消费。同名 remote 改指另一服务时须显式处理冲突，不接受已有 archive ref 的静默换源。它不覆盖 `refs/heads/traces`、`refs/libra/traces` 或本地 capture catalog。普通 clone/fetch 不自动探测、注册或下载 traces。通用第三方 Git 的 mirror/custom ref 语义维持当前契约；Mega2 code endpoint 的 `--mirror` 只镜像代码视图。

## 6. 普通代码 clone 与旧 Git traces

不能只隐藏 `refs/libra/traces`：当前 ImportRepo `full_pack` 忽略 wants、遍历全 repo objects，必须改成所请求且已授权代码 ref 的可达闭包，保留 annotated tag/peel、去重与准确 pack count；full/incremental/shallow/filter/v0/v2/HTTP/SSH 都遵守同一视图边界。普通代码入口不公布 traces ref，不将其选为 HEAD，不能通过已知 traces OID 的 want/delta 绕过边界。

对于旧 ImportRepo 同库保存的 `refs/libra/traces`，保留原始 Git 数据但从代码 advertisement/default-selection/pack 全量扫描中隔离；**不纳入旧 Capture purge**，不得凭未知 author/source 自动迁入某个新 owner stream。只有旧 traces ref 的仓库代码 HEAD 应为 unborn/无代码分支，不回退到 traces。对代码 ref 的祖先已经混入 traces commit 的污染，无法安全剪掉祖先而保持原 commit；必须报明确 `trace_history_in_code`、隔离该受污染代码 ref并要求管理者显式修复，不自动改写历史。与代码共用的内容 blob 可作为合法代码闭包的一部分；验收判断的是 traces 专属对象。

固定代码 refs 和内容，增加大量 traces 历史后，普通 clone 的 refs、pack 对象集与传输对象量应不变，本地 ODB 中不得出现 traces-only sentinel commit/tree/blob。仅“没有创建 traces ref”不能算通过。

## 7. 旧 Capture 全量清除与接口退场

旧 Capture 12 表及其全部所属对象（raw、redacted、staging 及可信清单确认的历史前缀）**全部删除，不迁移**；用户已明确选择这一策略。先通过 H5，再执行限定作用域、可恢复的清除流程，最后以 H6 放行终态。不能把所有共享 bucket 的 `agent/` 当可删除范围。

`mega2 traces purge-legacy-capture` 维护操作流程：建立跨实例持久 writer fence → 根据可信维护清单冻结获授权 deployment/tenant 拥有的 backend 指纹及全部旧对象前缀 → 持久 journal 保存 inventory/cursor/retry 与所有删除意图（包括旧作用域中的无 DB 引用 orphan）→ 分批删除并复扫 → 完成收据 → 守卫迁移 DROP 12 表。前缀必须覆盖旧 blob 的所有 visibility，而非仅 raw：登记 raw/redacted/staging、DB 中实际派生键及可信历史清单确认的遗留项；未知 ownership 或前缀阻断清除，不猜测扩大共享 bucket 范围。fence 须由 DB 级触发器/权限屏障阻止旧12表的 runtime DML，不能只让新二进制查询 fence；维护 journal/清除角色不被该屏障阻塞。还须停旧入口、冻结部署控制面、核对所有实例并等待既有 lease/in-flight 写入终止，以受控旧 image 重启与旧凭据写探针证明不能继续产生旧对象；有独立对象存储写权限时须撤销旧作用域 PUT 权限（保留维护删除权限）。只有 DB 屏障但旧进程仍能创建 orphan 对象不能放行 inventory。旧 DB 不记录历史 backend，维护清单须对照当前配置、DB、历史部署位置和运维声明，任一历史存储位置未核实就不能宣称全部清除。receipt 必须绑定冻结 inventory、backend、scopes 与 fence；变更作用域或存储后必须重新审计，不能沿用旧 receipt。

12 表容纳多个 deployment/tenant，整表 DROP 不得凭单个 scope 的完成收据放行。任何破坏性删除前，preflight 须证明该数据库全部旧 scope 均属于这次明确授权的维护集合；未获授权 scope 或不明归属存在即阻断，等待具名 owner 的可信维护交接。多 owner 仅在各自授权 scope 全部完成且跨实例 fence 覆盖同一数据库后，才能生成覆盖全库 scope 的 aggregate receipt；guarded DROP 重新核验没有集合外的旧行。共享 bucket 中不属于这组 scope 的对象、新 traces 和旧 Git traces 一律排除。

清除器用不自动 apply migrations 的 DB连接，首次建立维护 journal 后才能运行删除，避免普通 `database_connection` 提前执行 DROP。guarded DROP 仅注册到**显式维护 migrator**及独立 `traces_maintenance_schema_migrations` ledger，不注册到普通启动 `Migrator::up` 列表。它核验完整 receipt 后才执行并记账，未执行不得返回虚假已迁移状态。普通启动继续保留旧表，不因清除未完成拒绝新服务启动；不能靠 up 返回 Ok 来跳过 DROP 后却记已执行。空安装能以经过验证的空 inventory 放行。没有 bucket list/version 权限或版本能力不完整时，记录未完成原因并禁止完成 receipt/DROP。

当前对象删除接口不能证明云存储历史版本已清除；支持过的后端必须使用 provider 原生 version/generation inventory 与删除，删除标记也要核对。仅 current listing 为空不算“全部清除”。当前支持的 Local、S3/S3-compatible（包括 RustFS/R2）及 GCS 依实际后端分别验证；不新增 Azure 等未支持后端。凭据与真实 key 不写报告。云端历史版本恢复失败只能前滚修复，不能恢复已删数据或伪造 down migration。

DROP 依 FK 顺序删除 `agent_capture_session/event/source_stream/checkpoint/file_op/blob/blob_ref/ingest_receipt/stream_blob/access_audit/tombstone/deletion_ledger`。保留 `m20260913_000100_add_agent_capture_tables` 的迁移历史；新增 forward-only guarded drop，不能删除旧 migration 造成升级账本分叉。维护 journal/最终 receipt 不随旧12表删除。

退场还包括旧路由/DTO/service/storage/entity/config/OpenAPI、`agent_capture.events.committed` / `agent_capture.checkpoint.committed` 两事件及旧 scope 过滤项；旧配置显式报移除错误，不静默忽略。新 traces 读侧按已选能力独立实现，不能复用旧 raw-only / `missing_raw` 语义。旧设计文档在实际 H6 前保留为**现行实现**事实，H6 后标退役并指向本契约。history-projection 的未来 code/view 关联改为新 `trace_code_link` 承接，视图 P1/P2 仍按自己的计划。

## 8. 联合验收与发布证据

固定同一组服务/客户端 commit+version/fixture，覆盖：两种 transport 的有效与无效身份；同项目跨 principal/source/epoch、跨项目隔离；首次推送并发与回复丢失；FF/删除/中途 checkpoint 篡改；SHA-1/SHA-256 raw byte round-trip；跨 scope wants/delta base拒绝；普通 fresh/bare/mirror/shallow clone 不传 traces专属对象；已混存旧ref与错误HEAD；索引 crash/replay/unsupported/invalid 水位；关键词 CJK/ASCII/标点/part边界/超长行/重复片段；有界读取和原证据重读；跨项目搜索计数/游标权限隔离；旧清除中断续跑/历史version/共享bucket不误删/receipt-before-DROP。

没有真实服务运行或凭据时，依对应计划登记未运行与前置缺口，不把 fixture、过滤零命中或 provider skipped 记为 live PASS。分别记录 Git 已接收、读索引可查、联合实际服务通过、旧 Capture 全量删除及发布 C/D 的证据；计划文档提交不改变任何实施卡的 pending 状态。
