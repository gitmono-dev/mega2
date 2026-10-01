# Libra 协作：Agent 变更证据与可信交付重构需求

本文定义 mega2 为配合 Libra，将 Agent 的意图、可观测运行记录、候选修改与验证结果接入 CL／主干交付所需的重构。

> **状态**：重构需求，尚未实现；文档 review 的 PASS 仅代表需求可作为后续设计与实现的输入，不代表功能、性能或安全验收通过。
> **治理**：遵循 [general.md](general.md) 的文档结构；本次交付依用户要求仅编写及 review 需求，不改代码、不 bump 版本、不提交或部署。
> **强依赖**：与 [trunk-push.md](trunk-push.md) 共用根树写入串行化及 landing provenance；测试场景在 [integration.md](integration.md) 登记。公开分支、Tag 与 ImportRepo 例外见 [使用指南](../user-guide.zh.md)。

## 1. 目标、范围与非目标

目标是让团队能回答“谁因何需求修改了什么、在哪个精确版本上得到哪些验证、由谁批准、最后进入哪个主干版本”，并把有效历史回馈给后续 Agent 任务。产品最小闭环是跨服务修改的证据审查与合并把关。

mega2 的责任是中央接收、身分与授权、持久化、关系索引、证据有效性、批准及 landing；Libra 的责任是本机工作区、Agent adapter、捕获、脱敏、checkpoint 与上传。website frontend 消费 mega2 API 呈现 diff、证据及批准；本文只定义它所需的后端契约。

下列内容不在本次重构的核心交付范围：

- 取得模型未公开的内部 chain-of-thought，或保证摘要忠实呈现全部内部推理。
- 自建通用 coding agent、聊天产品、模型路由平台、IDE 或完整 CI runner／构建系统。
- 强迫客户迁移所有 Git 托管，或要求普通 Git 客户端理解 Libra 对象。
- 跨独立仓库的原子提交、数据库／外部 API 副作用的自动回退、模型运行的比特级重现。
- 把 transcript 数量、token 数或个人 AI 使用排行作为产品成功标准。

所有本文添加的字段、状态、API 能力与表名称均为**目标契约**；正式 URL、DDL、索引与错误码在阶段 0 的契约稿冻结，不能从本文示意直接宣称已有端点。

## 2. 事实校准（2026-09-06）

依据当前工作区静态源码：mega2 `Cargo.toml:3` 为 `0.5.3`，Libra `Cargo.toml:3` 为 `0.22.15`；未运行集成或性能测试。代码锚点以路径及符号为主，行号只描述本次快照。

| 能力／组件 | 实现状态 | 事实、锚点与限制 |
|---|---|---|
| Repo artifacts HTTP 面 | 已实现 | [`artifacts_router.rs:35`](../../src/api/router/artifacts_router.rs)：discovery、batch、commit、sets、读写对象。**目前没有 repo 授权保护**：`src/contract/policy/guard/cedar_guard.rs:52` 的 `resolve_cl_action` 只映射 `/cl`；artifacts 落入 `UnprotectedRequest` 并在 `:374` 直接放行，handler 无独立授权；不是可直接承载可信证据的安全入口 |
| AI artifact 类型 | 已实现 | [`artifacts/mod.rs:15`](../../src/contract/api/artifacts/mod.rs)：Intent、Run、Evidence、Decision、Provenance 等枚举；UUID OID 与 Git hash 不同 |
| 证据不可替换保证 | 部分完成 | [`artifact_service.rs:677`](../../src/jupiter/service/artifact_service.rs) 的 `commit_artifacts` 拒绝同 set ID 的不同 manifest；但 `upload_artifact_object_bytes`（`:233`）对既有同大小 UUID 仍调用 `put_stream`，不是内容不可变保证 |
| CL／队列 | 已实现 | [`mega_cl.rs:10`](../../src/callisto/mega_cl.rs)、目前的 [`push_queue_service.rs`](../../src/jupiter/service/push_queue_service.rs)；既有队列本身不能证明本提案所需的跨进程根树串行化保证 |
| 按任务隔离 CL | 未实现 | [`monorepo.rs:1250`](../../src/ceres/pack/monorepo.rs) 的 `fetch_or_new_cl_link` 仍以 path＋username 查找 open CL |
| CL 至 landing 的完整追溯 | 部分完成 | [`mono_api_service.rs:2582`](../../src/ceres/api_service/mono_api_service.rs) 的 `merge_cl_unchecked` 合成更新；`mega_cl` 无直接的 `merge_commit_id` 字段，尚缺本文要求的完整关系契约 |
| Bot 与授权 | 部分完成 | [`bot_router.rs:80`](../../src/api/router/bot_router.rs) 有 token 管理，`src/api/oauth/mod.rs` 有 `BotIdentity`；[`mega.cedarschema:1`](../../src/contract/policy/mega.cedarschema) 的 action principal 仍为 User。`src/contract/policy/enforcement.rs:71` 的 off／shadow 均放行，`config/config.toml:343` 的现行默认为 off；授权运行前置与完整 Agent 委托模型均需补齐 |
| Libra 捕获／模型 | 已实现 | [Agent 命令](../../../libra/docs/commands/agent.md)、[对象模型](../../../libra/docs/ai/object-model.md)：session、checkpoint、coverage、脱敏与 snapshot/event/projection 分层；外部捕获和 Libra 原生工作流的结构化程度不同 |
| 多来源中央汇聚 | 未实现 | [`Libra agent/push.rs:1`](../../../libra/src/command/agent/push.rs) 使用固定 `refs/libra/traces`，有 force-with-lease；不是多人中央事件摄取契约 |
| 大型工作区／依赖自动推导 | 部分完成 | Libra [sparse-view](../../../libra/docs/commands/sparse-view.md) 只过滤显示；[deps](../../../libra/docs/commands/deps.md) 为声明式边，不能当成完整 sandbox、按需 materialization 或自动依赖分析 |

校正既有分析时需注意：

1. mega2 已有 AI artifacts 与 bot 基础，不能将其写成全部从零构建。
2. `update_branch` 的空 diff 分支已将 `(from,to)` 一并移到 target head（`mono_api_service.rs:3353`）；不得沿用旧 [delta.md](../gap/delta.md) 的 no-op 回退推论作为尚未修复的现状。
3. 根写入的 FIFO、锁、CAS 与 storage-only trunk 行为在 `trunk-push.md` 是需求，不是已交付能力。本文不以其已实现为前提。
4. 两边 `Cargo.lock` 的 `git-internal` 分别是 `0.8.7`／`0.8.6`；不同版本不直接证明不兼容，仍需 wire fixtures 验证。
5. artifacts 源码注解引用 `docs/artifacts-protocol.md`，目前该文件不存在；阶段 0 必须创建正式契约文档或修正引用，不能以缺失文档作为现行规格依据。

## 3. 硬约束与不可违反的原则

1. **事实与解释分离**：观测事件、Agent 明示理由、事后生成摘要必须标记来源；缺少数据不得补造为事实，因为合并判定需要可核对证据。
2. **身分不由 payload 自证**：author、模型名、签名与 human delegator 不等同；服务器验证授权后绑定 principal，避免冒名及自行提权。
3. **历史追加，投影可重建**：修订、否决、失效、撤销与删除留存指针均有事件；正常编辑不覆写历史。敏感内容依删除政策移除，见 REQ-LB-09，不能用不可变性阻止合法数据治理。
4. **内容 digest 与逻辑 ID 分离**：UUID 表示身分，digest 表示字节完整性；签章／服务器收据只证明来源与接收，不能证明测试或主张正确。
5. **代码、证据、批准精确绑定**：不得将旧 patch 的成功检查套用到新 patch；不能只用 CL link、时间戳或可变分支名做关联。
6. **共用根写入者边界**：集成 `trunk-push.md` 的单一根写入服务，不另建并行写 main 的 Agent 队列；否则证据检查与写入间会出现竞态。
7. **普通 Git 与 ImportRepo 行为不暗改**：不添加公开 heads，不开放客户端 tag，不把固定 traces ref 默认套进 monorepo 的 CL 写入路径；扩展必须 discovery 协商。
8. **选择性启用，保护模式不降级**：未启用集成的仓库保持既有 Git 行为；不承诺保留未授权读写 evidence 的旧 artifact 旁路。新 evidence 服务即使采 observe 也必须运行授权；一旦 gate 启用 required，缺证据、依赖服务失败或不兼容客户端均不能被当成 pass。
9. **最小权限贯穿派生数据**：列表、摘要、搜索、下载、通知、索引与缓存都适用；代码可读不自动授予原始 transcript 可读。
10. **复现承诺分层**：可恢复快照、可查看轨迹、可重跑固定测试分别声明；外部工具记录是数据，不得在查找／回放时自动运行。

## 4. 现状与目标对比

| 维度 | 当前状态 | 目标状态 | 实现难度 |
|---|---|---|---|
| 工作单元 | path＋username 隐式选 CL | 稳定 change ID，明确 task／run／CL revision | 复杂 |
| 中央同步 | 通用 UUID artifacts；Libra traces ref | 可协商、幂等、可恢复的证据摄取 | 复杂 |
| 证据可信度 | 有类型及 metadata | 来源分级、内容验证、不可覆写、完整性诊断 | 复杂 |
| 合并把关 | 既有 CL checks／review／queue | 对精确候选树与政策的证据评估，写入时重查 | 复杂 |
| 历史追溯 | commit／CL／artifact 分散 | 原始 patch、rebase、批准、landing 的持久关系 | 复杂 |
| 下次任务上下文 | 本机记录为主 | 按权限、版本与有效性返回可引用历史 | 中等 |
| 市场采用 | 自有托管后端为主 | 原生 monorepo 模式＋可选外部 SCM 证据服务 | 复杂 |

## 5. 领域契约与数据所有权

以下为逻辑模型，不要求每一行都添加一张表；实现应复用 `callisto`／`jupiter`，避免拷贝 Libra runtime DB。

| 逻辑实体 | 稳定识别及必要数据 | mega2 责任 |
|---|---|---|
| RepositoryIdentity | 服务器发行的 repo ID、deployment／tenant scope、外部 provider repo ID（可选） | remote URL 只是可变别名，不作授权或租户主键 |
| Change | repo ID＋change ID、owner principal（含类型与稳定 ID）、created-by principal、可选 delegator／grant、可选 intent／task ID | 一个工程变更；可有多个 run、多个 CL revision；owner 默认为经认证的创建主体，委托者不自动成为 owner |
| CLRevision | change ID、既有 CL link、revision ID、base／tip／tree IDs、root baseline、path mapping | CL 本体添加 typed owner 与不可变的 selector mode（legacy／explicit）；revision 保留 actor／delegator 引用，不覆写旧修订；更新用 expected revision CAS |
| Run／CaptureSource | producer instance、provider session、run ID、collector／adapter 版本、capture coverage | 外部不完整捕获可只有 session，不强制生成虚假 Intent／Plan |
| EvidenceBundle | bundle ID、schema version、对象清单／digest、run／patch revision、脱敏版本与完整性状态 | 管理接收、验证、提交与幂等重送 |
| Verification | subject fingerprint、check ID、环境／工具链、结果、runner 身分、起止时间 | 分辨 Agent 自述、collector 观测、受信任 runner 结果 |
| Approval | reviewer principal、subject fingerprint、policy revision、决策与时间 | 以事件追加批准、否决与撤销；不信任 payload 自报批准者 |
| LandingRecord | change／CL revision、原始 OID 集、候选与落地树、path／root commits、queue operation ID | 与实际根 ref 推进一致提交，支持一对多／多对一关系 |
| Projection／Outbox | 投影版本、处理水位、重送 key、错误状态 | 支持可重建查找及对外通知，不作历史真相 |

`subject fingerprint` 至少包含：repository ID、CL／patch revision、base root commit、候选 root tree、subtree path mapping、依赖／lockfile fingerprint、测试规格、环境与工具链 fingerprint。policy revision 另作批准／评估的绑定键，不能在政策变动后继续沿用旧评估。

只知道子树 tip 不能证明整个 monorepo 已通过验证。组装候选根树时必须验证路径映射、子树来源与授权；路径要做组件边界范式，拒绝 `..`、编码绕过、前缀碰撞及跨 ImportRepo 边界的隐式映射。

## 6. 重构需求

### REQ-LB-01 — 版本化摄取协定（P0）

**改造面**：`src/contract/api/artifacts/`、`src/api/router/artifacts_router.rs`、`src/jupiter/service/artifact_service.rs` 与其 storage/migration；必要时添加相邻 evidence 契约域，而不是在任意 metadata 中隐藏必需字段。

- Discovery 必须报告 supported schema、hash algorithms、limits、授权方法、上传模式及是否支持证据验证；不能因通用 artifact 类型存在就声明 Libra-ready。
- 兼容 v1 UUID OID；添加 evidence schema 以版本区隔，禁止把 OID 静默改成内容 hash。未知必需版本／算法在写入前拒绝；未知可选字段按冻结规则处理。
- 摄取流程为 negotiate → staging upload → finalize／verify → committed receipt。只有 committed、完整且授权有效的 bundle 可以成为合并证据。
- 幂等键以 server scope＋repo ID＋producer ID＋bundle ID 为界；producer ID 由服务器登记并绑定已验证 principal，禁止 client 冒用其他 producer。查找／重送先授权再比较；同键同内容返回既有收据，同键不同内容冲突，不以 last-write-wins 覆盖。
- 允许事件乱序、重复与脱机续传；保存每来源序号／缺口及 server receive time，不假设全域时钟顺序。缺关联的数据标记 incomplete，不生成假的因果关系。
- 部分缺件返回可恢复的 missing set；未授权 OID 的存在性不得泄露。限制压缩前后大小、对象数、单请求／单租户配额与并发，支持背压。

**验收**：AC-LB-01／02／03。Libra 真客户端对接是完成条件；只有手写 JSON 的成功测试不算端到端完成。

### REQ-LB-02 — 内容完整性与保存发布（P0）

- digest 对明确定义的、已脱敏字节计算，manifest 的 canonicalization、算法名称和大小均版本化；server 自行计算／验证，不只相信客户端 header 或 object-store ETag。
- 所有写入模式，包括 proxy PUT、signed PUT、multipart，先写唯一 staging key；检查内容后以条件创建或等效不可覆写机制发布至 final key。已发布对象不得再发可覆写的 signed PUT。
- final storage key 必须包含部署／租户隔离域与 content identity，跨 repo 共享只在明确授权及 reference accounting 下启用；对外的 UUID namespace 不构成权限。
- manifest、关联及收据在 DB 事务内提交；对象先验证可读再发布 manifest。对象已写、DB 未提交属可回收孤儿；DB 已提交、回应遗失应可幂等查回。GC 与 finalize 需锁／租约协作，不能删除正在发布的内容。
- 旧 artifact 标为 legacy-unverified；授权导入后以新 evidence manifest／final key 固定一份已验证内容，才可用于新 gate，不直接采信仍可由 v1 修改的保存引用。不得伪造历史接收时间或验证者。
- v1 兼容只保证 UUID／wire 形状，不保证匿名可读写 evidence。新 evidence 命名空间及内容必须在所有旧 batch／PUT／GET／manifest 界面被授权隔离或拒绝，亦不得藉相同 UUID、旧 signed URL、legacy GC 触及 final evidence；旧端点不能成为旁路。若部署共用可达的底层 key，先隔离／迁移并验证旧 URL 失效，再允许启用证据服务。

**验收**：AC-LB-02／03／12。对同 UUID、同大小、不同内容及并发 PUT 逐一验证，不能只测 size mismatch。

### REQ-LB-03 — 任务识别与 CL 修订（P0）

- 增加显式创建／绑定 change 的服务契约；每个 change 的 CL 修订可由多个 run 贡献，不把 agent session ID 等同 change ID。
- 新客户端必须显式指定已授权的 change／CL target 与 expected revision；协定 carrier 在阶段 0 决定并由 discovery 声明，不能假定现有 Git server 已接受 push-option。
- CL owner 是 typed principal，created-by 与 delegator 分别保存。Agent／service principal 创建的 CL 默认 owner 为该服务主体、selector mode 为 explicit；不得把 delegator 的 username 填进 owner 以冒充人。人工以新 API 创建的任务也为 explicit。转移 owner 需明确授权及审计，不改 selector mode。
- 普通 Git 的隐式候选集合**只含 legacy mode、同 repo/path、同已解析人类 principal 的 open CL**，不包含 explicit Agent 或新任务 CL。零项按原流程创建 legacy CL，一项更新，多项拒绝歧义并给出选定目标的方法；必须替换目前 `.one()` 的不确定查找，不能任选最新 CL。明确指定 CL 的更新另验证 target 权限。
- 同用户同路径添加 Agent 任务不得污染 legacy 候选集合。既有匿名／无法解析身分列的兼容映射见 §8，不能将字符串相同当成经验证的人类身分。
- 同路径多任务可同时存在。`trunk-push.md` ADR-TP-10 的每路径一项在队**仅约束 `kind='push'`**；CL 落地为 merge 行，由队列全序与锁内重查串行化，本文不添加亦不放宽 merge 的入队约束。
- rebase、squash、人工修改及撤销添加修订与 lineage 边；被淘汰修订在保留政策内可读。明确表示一个 run 产出多个 patch、一个 patch 混合多人／Agent 的情况。

**验收**：AC-LB-04／05。需覆盖 Git HTTP／SSH 与 Web 编辑三个入口的一致性。

### REQ-LB-04 — 委托身分与证据授权（P0）

- 在既有 BotIdentity／token 基础上创建 service principal 与 delegator 关系，Cedar schema／entity builder／HTTP／Git 解析与审计同步更新，不是只添加 `Bot` 名称。
- **启用前置**：添加 evidence 服务默认关闭；启用时必须同时满足 `cedar.enforcement=enforce`、证据端点完整 action／resource 登记、身分与 policy store 可用、v1 旁路已隔离。启动及 reload 均校验；off／shadow 与启用 evidence 互斥，拒绝不安全组合，不能先接收再期待 gate 挡住。gate 的 off／observe／required 与 Cedar enforcement 是两组不同设置。
- evidence 的 negotiate、upload、finalize、查找、approval 与 landing gate 必须有专用 fail-closed 授权路径；不能落入现有 `UnprotectedRequest` 放行分支。未知 action、entity store 缺失、resolver 失败一律拒绝。required 的前置还包括 evidence 服务已启用且授权真正运行；授权失效时拒绝新操作并阻止 landing，不自动降级。
- 授权范围至少有读代码、写候选修改、提交证据、读原始 trace、读衍生摘要、批准与运行 landing。批准身分不能由 Agent 上传 Evidence 冒充。
- 有效权限为服务主体、委托授权、repo/path policy 及任务限制的交集；短期 token 带 audience、expiry、grant ID，可撤销，不嵌入 transcript。未委托的 service job 必须有明确 service grant，不能把缺 delegator 当作不限权。
- 接收、finalize、查找与 landing 均按当前授权判定。委托过期不能以已入队规避；历史记录保留原 principal，后续操作需重新授权。
- 跨 repo transcript 以分片及分类处理；无法可靠切分时采取足够严格的可见范围。衍生摘要不能绕过来源权限，搜索计数及缓存亦适用。
- signed download 有最长 TTL 与明示撤销窗口；要求实时撤销的数据只能经每次授权的 server proxy，不宣称已发出的签名 URL 可立即回收。

**验收**：AC-LB-06／07／13。添加 action 与缓存版本失效必须做负向权限测试。

### REQ-LB-05 — 验证／批准有效性与合并门（P0）

- 将捕获完整性、来源信任、内容完整性和测试结果分开存储；例如 incomplete trace 并不等于 failing test，但 required coverage 缺失必须阻止 gate。
- 结果至少区分 passed、failed、missing、stale、unverified、error；同一 fingerprint 上可有多个独立 check，不让最后一份 Agent 自述覆盖受信任 runner 的失败。
- 定义版本化 policy：`off`（新 gate 关闭，既有 checks 不变）、`observe`（只记录新判定）、`required`（缺必需证据拒绝）；切换由授权管理者操作并追加审计。紧急例外需独立权限、理由、范围和有效期，不能假冒 passed。
- 明定自审批政策：需要人类批准的 required policy 默认禁止 owner、created-by、实际修改者及已记录 delegator 充任唯一批准者；允许自审批须由授权管理者在版本化政策中显式启用并可查找。既有 ACL 自提权与 reviewer 硬限制优先，不得由此设置削弱；service principal 不能提供人类批准。
- patch、依赖、测试规格、环境、批准対象或政策改变即重新评估。首版保守策略：root baseline 改变使候选集成证据 stale；局部证据重用是后续可选能力，需完整输入闭包证明。
- 受信任 CI 提交使用 runner principal 与不可重放的 job／attempt 关联，server 验证实际受测 commit/tree；“某 pipeline 成功”不能直接映射为当前 patch 成功。
- required policy 必须覆盖所有能落地的入口，包括一般 merge、内部 merge、queue processor。直接写 ref、Web 写入与 ImportRepo attach 是否影响受保护范围，需依根写入清单接同一 gate 或明确拒绝，不能留下旁路。

**验收**：AC-LB-05／08／09。旧客户端在 off 下不受新 gate 阻挡；required 下返回可操作的 missing／unsupported 诊断。

### REQ-LB-06 — 原子 landing 与主干追溯（P0）

- 复用 `trunk-push.md` 阶段 1–3 的根写入 FIFO／事务锁／CAS。本文不重新定义队列顺序、CAS 失败重试或 root writer 名单；与该文不一致时先修订协同设计。
- 耗时测试在根写入临界区外运行；进入写入轮次后重查 root baseline、CL revision、授权、policy、证据状态。若已 stale，退出本轮并重新验证，不能锁住根等待 CI。
- 主干 ref、landing mapping、被接受修订、必要审计和 outbox 必须在同一 DB transaction 内持久化；不能先推 main 再 best-effort 写溯源。
- LandingRecord 记录原始 commit 集、path commit、root roll-up、candidate fingerprint、批准及验证引用。commit trailer 可作导航但不是唯一权威；source commit 不可达后仍需保有符合政策的内容与关系。
- crash／重送按 operation ID 返回同一结果，不重复合成 commit。no-op／净零结果明确记录为无新根 commit，不能臆造与 queue ID 一一对应的 landing SHA。
- 未来 storage-only trunk 模式不创建虚构 CL、人类批准或 website session；可记录 transport principal 及 provenance，但不宣称提供本文 required 审查保证。若要在该模式启用 required，需额外定义服务身分／政策前置并完成独立验收，首版拒绝此组合。

**验收**：AC-LB-09／10／12。含两个 server process 的并发场景及 DB 提交后回应遗失场景。

### REQ-LB-07 — 评审与历史查找 API（P1）

- 可由 commit、CL revision、change、run、repo/path 查找完整关系；返回来源定位、coverage、信任、有效性与缺失原因。unknown 保持 unknown。
- website frontend 所需契约包括 diff 对应意图摘要、被否决方案、测试版本、批准／撤销及 landing；不要求 reviewer 默认读取全部 transcript。
- 所有摘要保留来源 bundle／对象／事件位置、生成器及版本；人类批准与模型建议用不同类型表示。
- history retrieval 按 revision、依赖 fingerprint、有效／否决状态及当前权限过滤。被否决方案可作反例，但不得标成推荐；新任务不自动运行历史命令。
- 以 keyset cursor 分页、指定投影版本／读取水位；投影可重建，落后时返回 pending／watermark，而非以空结果暗示无证据。合并门读取权威状态，不依赖最终一致的搜索索引。

**验收**：AC-LB-07／10／11。website frontend 变更需另行纳入实现写集、同栈测试与 README 所载 rebuild/reload 工作流；本次不修改前端。

### REQ-LB-08 — 影响分析与外部 SCM 模式（P2，可选扩展）

- 通过 adapter 消费 Nx／Bazel／Buck 等既有输出的依赖／测试图，固定图版本、来源 commit、生成器及完整性；不将 Libra 手工 deps 图当成完整真相。
- 依赖图缺失、不支持语言或输入闭包不完整时，required 模式要求保守测试集合或授权人工处置，不能把空图当成无影响。最小闭环可使用管理者配置的固定必需测试集合。
- 外部 GitHub／GitLab 模式只提供证据与 checks；以 provider installation＋immutable repo ID＋PR/head SHA 关联，verify webhook、deduplicate delivery、处理乱序并回查当前 head。对外写 check 需客户明确安装授权。
- 外部合并是否受控，取决于 provider branch protection／required check 配置，必须标示已验证／未验证 enforcement；没有权限确认时只能声称观测。不得假称 mega2 对外部 SCM 具有原子落地主权。
- 不能以先完成多 provider adapter、通用矢量搜索或全量工作区 provisioning 阻塞原生 CL 的最小闭环。

**验收**：AC-LB-14／15；可选 adapter 未交付时 discovery 明示 unsupported。

### REQ-LB-09 — 保留、删除、成本与恢复（P1；敏感数据前置为 P0）

- 原始 transcript、脱敏内容、衍生摘要、验证证据与最小审计索引分别定义 retention；首版默认只收脱敏内容，raw 必须独立 opt-in 及权限。脱敏失败的 payload 不可发布。
- 删除以可审计 tombstone 表示，按引用、legal hold（若配置）与保留政策删除内容；摘要、搜索索引、缓存、导出副本和备份恢复均要纳入。已删内容不得被重新导入／备份重播复活；需要 deletion ledger 及恢复水位。
- 历史已 landing 的合并事实保留，内容到期显示 expired／deleted；不回头伪造“当时未验证”。尚未 landing 的 required 证据若失去可验证内容，阻止合并。
- GC 区分 staging lease、committed reference、active verification、landing retention 和 deletion tombstone；不得只以 blob 年龄判断。授权隔离域内去重，限制大小、频率、保留量及索引增长。
- 定义备份一致性点、对象／DB／deletion ledger restore 顺序、重建验证与恢复演练；恢复到无法证明完整性时只提供受限读取，不启用 required landing。
- 指针包括接受／拒绝／缺件／stale 数、上传与索引延迟、每变更保存量、GC／outbox backlog、追溯成功率；不得在 telemetry 输出原始 prompt 或 secret。

**验收**：AC-LB-03／07／11／12／13。无 raw 隔离与删除保证时，不开放 raw 接收。

## 7. 前置依赖矩阵与决策边界

| 本文工作 | 依赖 | 类型 | 关键同步点 |
|---|---|---|---|
| LB-01／02 | 现有 artifacts／orbit；[contract.md](contract.md) 边界 | 前置 | 固定 wire schema、UUID 兼容及发布原子性；不重新拆 orbit package |
| LB-03／06 | [trunk-push.md](trunk-push.md) 阶段 1–3、ADR-TP-10/14/15/16 | 前置／协同 | 共用 root writer 和 provenance；ADR-TP-10 只限制 push 行，merge 不添加路径唯一约束；no-op 无新根 commit |
| LB-04／05 | 既有 Cedar、BotIdentity；[website-auth.md](website-auth.md) 的认证分工 | 前置 | 新服务身分不另建登录面；政策缓存、各入口及撤销一致 |
| LB-01／04／09 | [config.md](config.md)、[vault.md](vault.md) 现行配置／SecretRef 约束 | 协同 | 使用既有启动与凭据加载次序；添加 config schema／validation／reload 需同步既有规则 |
| LB-07 | website frontend API 消费 | 后置 | 后端契约先冻结；前端不读 artifact store 或 DB 旁路授权 |
| LB-08 | 既有 CI／SCM provider adapter | 可选 | 固定测试集合即可完成首批 gate，不依赖 Orion 整体移植 |
| 全部 | [integration.md](integration.md)、[test-infra.md](test-infra.md) | 协同 | AC-LB 场景登记、migration 真实建库、双进程与真 Libra 客户端 |

以上 config／vault／website-auth／contract 为遵循既有约束，本文不改其设计或添加反向阻塞。添加的强协同（trunk-push、integration）已在对应文档及 README 登记。若实现需改既有硬约束，必须在该阶段同步修订受影响文档。

阶段 0 必须形成可 review 的契约决策：① Git target carrier／legacy 歧义错误；② evidence schema／canonicalization／error codes；③ 身分及 policy entity 迁移；④ root writer 事务与 provenance schema；⑤ 上传 backend 条件发布能力及降级拒绝规则；⑥ retention／原始数据分类；⑦ schema upgrade／rollback 边界。本文先固定上述行为与验收，未决的实现细节不能由单一模块暗中决定。

## 8. 迁移步骤（依赖顺序，不承诺工期）

| 阶段 | 具体交付 | 前置 | 可验证出口 |
|---|---|---|---|
| 0：契约冻结 | 校准源码与现行计划；冻结 §7 七项决策、OpenAPI／fixtures、数据分类与错误语义 | 本需求 review | fixture 能表达 partial capture、mixed authors、子树、no-op；新旧客户端矩阵无未定义格 |
| 1：可信接收 | LB-01／02、LB-04 接收读取所需权限、LB-09 基础隔离／保留；schema 用 additive migration | 0 | AC-01（至 bundle 查回）、02/03、06（接收／读取）、07（既有读取／下载面）、13（摄取／保留子集）；legacy 不能冒充 verified |
| 2：任务与 lineage | LB-03、LB-07 基本查找；为旧 CL 创建 legacy change mapping | 1 | AC-01（含 CL 查找）、04、05/10 的修订及来源查找子集；不要求尚未交付的 gate／landing 通过 |
| 3：主干把关 | LB-05／06；复用已验收的 root writer；先 observe 对照再逐 repo 启用 required | 2＋trunk-push 1–3 | 完整 AC-05/06/08/09/10，AC-12 的 landing crash／migration 子集及 AC-13 的未落地 gate 子集；所有落地入口无旁路 |
| 4：团队历史与运维 | 完整 LB-07／09；website frontend 契约、删除／restore／投影重建 | 3 | AC-07/10/11/12/13；版本追溯与删除不复活，评审可看对应证据 |
| 5：可选接入 | LB-08 依赖图与外部 SCM adapter，按独立能力开关逐一启用 | 4 | AC-14/15；外部 enforcement 不确定时不声称已保护 |

表内 AC 数字均指 `AC-LB-*`。阶段 1–3 的子集验收不能声称整项 AC 已完成；阶段 4 出口需重跑并完整通过 AC-LB-01…13。阶段 0 契约稿须将子集对应到具名测试，未交付界面标为 pending，不用软跳过当成功。

**升级与恢复要求**：

- 先扩展 schema 再回填；既有 CL 一律设置 selector mode=legacy，保留原 username 供审核。能从权威人类帐户映射验证者回填 typed human owner；已知匿名列回填 typed anonymous；无法验证的列为 legacy-unresolved，不伪造 human/service principal。legacy-unresolved 仅在未启用新集成的旧模式保留原查找行为；受保护 repo 启用前必须由管理者解析归属或隔离这些列，resolver 不得在运行时因同名自动认领。不得从相近时间推造 run→commit 关系。逐批校验数量、引用及 digest，回填可重入。
- off／observe／required 是政策状态，不是软件版本判定。只允许管理者显式切换，记录原因；回滚二进位不得自动将 required 降为 off。
- 新旧 server 并行必须通过能力／schema 版本 fencing，旧节点不得写入受新 gate 保护的 repo；无法 fencing 的部署先排空写入再升级。
- 迁移失败不发布能力；已写新历史不做破坏性 down migration。故障回退优先关闭新摄取／排空 queue、保留唯读查找，再以前向修复恢复；不可用清空 artifacts 或重写 main 作回滚。

## 9. 集成测试与验收矩阵

以下 `AC-LB-*` 是需求验收 ID，不是已存在的 cargo target。实现时在 `tests/` 注册真实 target、加入项目测试索引／CI；共享 `integration.md` 的 DB migration 和 compose 基础。

| ID | 场景 | 必须断言 |
|---|---|---|
| AC-LB-01 | 真 Libra 捕获→上传→CL evidence 查找；混合版本／未知 schema | 三个身分（session、bundle、change）可追溯；同内容重送一份收据；不支持版本在写入前拒绝 |
| AC-LB-02 | 同 UUID 同大小异内容；proxy／signed／multipart；并发 finalize；冒用 producer／v1 路径 | 旧内容不可替换；digest 不符无 committed manifest；不同租户及同租户无权 producer 不互相探测；v1 不可触及 evidence key／manifest |
| AC-LB-03 | 上传中断、事件乱序、缺件、finalize／GC 竞态、配额 | 有限重试与 missing 诊断；缺件不能 pass；有效 lease 不被 GC；背压不产生半份 committed 数据 |
| AC-LB-04 | 同用户同路径两 Agent；HTTP／SSH／Web 更新；旧 Git；legacy 回填 | 指定 change 分离；stale expected revision 拒绝；legacy 多候选拒绝且有选择方法；新 Agent CL 不改变原本唯一 legacy CL 的成功推送；未解析 owner 不被冒名认领 |
| AC-LB-05 | rebase、人工补改、squash、被否决 patch、root 前移 | 旧 revision 及来源可查；指纹变动使证据／批准 stale，不可静默继承 |
| AC-LB-06 | 冒名上传、越权 path、过期／撤销委托、混合 Bot/User、Cedar off／shadow | payload 不能决定 principal；接收、finalize、落地重查；off／shadow＋evidence 启用在 startup/reload 拒绝，不得接收并采信证据；未知 route/action 不可放行 |
| AC-LB-07 | 无 trace 权限但有 code 权限；摘要／搜索／下载／缓存；v1 旁路／未运行授权 | 派生面与 legacy URL 不泄漏 evidence；off／shadow 不能经旧端点读／改 evidence；实时撤销走 proxy；到期 signed URL 不能再用 |
| AC-LB-08 | 自述 passed、runner failed、错 commit、伪 CI callback、政策更新、自审批 | 结果不互覆盖；只接受正确 runner＋subject；required 拒绝 missing/stale/error/unverified；delegator 自审批默认拒绝、例外政策可追溯且不绕过 ACL 硬限制 |
| AC-LB-09 | 双 server 并发 landing、root 前移、一般／内部／queue 入口 | 使用同 root writer；stale 不落地；授权／policy 在写入时重查；无旁路及重复 commit |
| AC-LB-10 | path commit／root roll-up／多 commit squash／no-op 反向查找 | 多对多 provenance 完整；no-op 无假 SHA；原始修改与最终树可校验 |
| AC-LB-11 | 投影清空重建、乱序 outbox、陈旧搜索水位 | 重建结果一致；不重复通知；gate 不依赖落后索引；未知／缺失明示 |
| AC-LB-12 | DB 前后各 crash point、回应遗失、备份还原、migration 中断 | main 与 landing mapping 同成败；收据幂等；deletion ledger 先于对外恢复生效 |
| AC-LB-13 | 脱敏失败、保留到期、删除、hold、共享 blob、旧 bundle 重播 | raw 不泄漏；活引用不被误删；被删内容不复活；未落地证据失效则阻止 gate |
| AC-LB-14 | 不完整／错 revision 依赖图，跨服务修改 | 空图不等于无影响；使用固定保守 checks；未知图不能减少 required checks |
| AC-LB-15 | 外部 SCM webhook 重播／乱序、PR 新 head、权限撤销 | 旧 check 不代表新 head；错签拒绝；缺 branch protection 时显示 unenforced |

实现阶段依 `AGENTS.md` 运行 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test --all`；涉及 `src/` 另跑 `cargo build` 与 `cargo build --tests`。不得用 ignore／软跳过把未验证的真客户端、双 server 或外部集成报为通过。本文文档交付只验证内容、链接及 review，不宣称上述代码门禁已运行。

## 10. 风险与约束

| 风险 | 影响 | 缓解措施 |
|---|---|---|
| 证据存得住但内容不可信 | 误把自述当成测试通过 | 分级来源、server digest、受信任 runner、required policy |
| queue／landing 双重建设 | 根写入竞态及溯源分裂 | LB-06 强依赖既有 trunk-push root writer，统一事务 |
| 过度保守的 stale 策略 | 主干活跃时重测与排队成本上升 | 首版正确性优先，量测 stale 比例；后续以可证明的输入闭包重用 |
| 捕获 schema 与模型工具快速变动 | 静默丢失事件、错误完整性声明 | adapter／schema version、coverage、golden fixtures、unknown 保留 |
| raw／摘要外泄或删除后复活 | 客户失去数据控制 | raw opt-in、分层权限、deletion ledger、restore 演练 |
| 功能扩散到完整 Agent 平台 | 内核闭环迟迟无法交付 | 内核阶段 1–4、可选阶段 5 分开；固定 CI checks 可先落地 |

约束另列：不能改变 monorepo 既有公开 Git 规则；不能把 storage-only 部署强接 website；不能在新 gate 出错时自动降级；不能把证据审查 PASS 宣称为软件正确性或法规认证。违反这些边界会破坏既有部署或产品承诺，需先修订对应契约并 review。

## 11. 多维评估

以下数字是需求设计评估，并非产品实测评分或对交付成熟度的背书。

| 维度 | 评估结论、当前不足与改进方向 |
|---|---|
| 合理性 | 9/10：直接连接捕获与合并；仍需真实团队验证评审需求，避免只展示 transcript |
| 可行性 | 7/10：已有 artifacts／CL／授权底座；跨域事务与 root writer 尚需实现，分阶段验收 |
| 完整性 | 8/10：含同步、授权、迁移、删除与回复；wire 细节仍待阶段 0 冻结 |
| 安全性 | 7/10：明确 trust／raw／撤销边界；需以负向测试证明，不能只靠 schema |
| 功能正确性 | 8/10：精确版本绑定及 no-op 语义明确；需跨入口／多对多 lineage fixtures |
| 可靠性 | 7/10：有 outbox、幂等与 crash 要求；实际 crash／双 server 测试尚未完成 |
| 兼容性 | 8/10：保留 v1 OID、legacy Git 和 off 模式；新旧节点 fencing 必须补齐 |
| 可扩展性 | 7/10：有 staging、配额、分页及可重建投影；容量／延迟目标须由试点基准决定 |
| 数据治理 | 7/10：有分层保留与 deletion ledger；hold／备份范围需客户政策配置及演练 |

## 12. 小结与预期收益

本重构把 mega2 的既有 artifacts、CL、授权与主干写入接成 Agent 变更的中央证据链。Libra 提供开发过程的来源数据，mega2 确保数据与特定候选版本、批准及 landing 正确关联。实现先完成可信摄取、任务隔离与原生合并把关，再扩展历史检索及外部 SCM。根写入、身分与数据治理沿用既有专题约束，避免形成互相竞争的事实源。

可观测收益：评审人工时间、一次验证通过率、stale 检查拦截数、合并后返工、从 landed commit 还原来源的成功率与耗时。先以同类型／难度任务创建基准，再观测导入后差异；不得把所有变化直接归因于本系统。完成标准是团队在真实变更中反复使用这条流程，不是保存更多 token。
