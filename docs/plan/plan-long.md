> **MTR-97 产品发布门已闭合（2026-10-24）：** 新源码8AC/3VER、A/B与正式本地Claude ER05/H0均PASS；本机完整C为20 targets/3041通过/0失败/3原有ignored、fmt/strict clippy/双build全exit0。C1缺runner101、C2外层预算124及STOP保留，C3仅按用户“授权”补齐build-tests一次并exit0。signed+DCO提交`04a206a`、main、标注`v0.42.66`及人工Release均实核；tag Docker `40059700063`的ARM64/AMD64/manifest三job与main Harbor `40059605942`全部success，未使用Harbor豁免。Mega产品卡完成数11/110（活动11/109），Libra0/26（活动0/25），联合11/136（活动11/134）。本次仅七文档收口候选；收口审查R1为FAIL，H0事实PASS，要求收口文档最终树完整五门C。授权前快照：该C及精确治理提交尚待，用户原“授权”仅限已执行C3；新C4窗口须另行明确授权。最终执行收据固定为`/tmp/mega2-mtr97-closeout-C4-20261024/C-terminal.json`，只在真实exit0且无漂移后才准本提交生效，不提前认定它存在或通过。旧C未被宣称覆盖新增文档。下一代码前沿FIX-MTR-07须待此收口，后续98→101→99→11及H1/LTR01边界不变；Libra治理仅本地、不push。以下旧快照按其时点保留。

> **MTR-97 已发布、远端待验（2026-10-24）：** 用户明确“授权”一次仅build-tests补齐窗口；本卡C3实际exit0/161.17秒、0警告/0错误，922输入及.env.test/工具链未变，沿用已通过四门。五项本地C为20 targets/3041通过/0失败/3原有ignored及双build全绿；C1缺runner101、C2外层预算124与原STOP均保留。精确signed+DCO提交`04a206a`、main推送、标注`v0.42.66`及人工Release已实核；Docker `40059700063`、main Harbor `40059605942`仍在运行，卡为in-progress/remote-pending，不记完成。Libra仅本地signed治理，LTR01/H1前不push；完成数仍Mega10/110、Libra0/26。

> **MTR-97 环境恢复审查通过（2026-10-24）：** 真实本地Claude session36bb023f/exec95192 exit0，对一次C恢复、原代码ER05沿用及H0同步均明确PASS，P0/P1=0；18输入hash核同。仅固定设施与机械0.42.66版本变化，源码/SSOT未改；不表示C或D通过。root接受P2：Libra新事实精确签名本地提交（owner=root/LTR01、不push）；此前未运行的后续target若失败须新证据并STOP（owner=root）；Cargo.lock相对正式前像仅mega2版本一行的实际diff已保存，若1800秒到期记timeout且不重跑（owner=root）。首次64失败及全部旧STOP保留。

> **MTR-97 首次完整C失败及环境恢复（2026-10-24）：** 0.42.66首次完整测试exit101，`integration_git_cli`为3通过/64失败，64项均缺规范`mega2-it/git-cli` runner；此前单元2739/0/3原有ignored、trunk80/80等通过不代全量C。原始日志与SHA保留，无源码改动、无新提交/推送/发布。root按测试基础设施约定恢复规范项目及固定Git/LFS runner，探针通过后只再执行一次原样完整C；若再次失败先保存并停止，不自动重试或降低门。原正式Claude代码/H0 PASS仅绑定原输入，环境恢复与新事实待真实本地Claude复核；Libra治理仍仅本地，LTR01/H1前不push。

> **MTR-97 当前前沿（2026-10-24）：** 正式本地Claude单审ER05/H0同步均PASS，P0/P1=0；新A/B已通过，进入一次patch0.42.66的完整C。卡仍in-progress/locally-accepted，提交/推送/tag/Release/D均待；Libra0/26、本地治理不提前push。详见[日期计划](plan-20261015.md)。

> **最新事实前沿（2026-10-23 MTR-96 实际完成）：** MTR-96 已 done/complete：内部 PACK 结构解码器 `ceres::traces::pack_decode` 落地，8AC/3VER 核销；ER-05 经 6 轮收敛为真实 Claude/隔离 Codex 双 `VERDICT: PASS`；bump 0.42.65 后完整 C 为 20 targets、3028 passed/0 failed/3 原有 ignored，fmt/clippy/双 build 全 exit0；提交 `6c4d208`、tag `v0.42.65`、人工 Release 与 tag Docker run `39955760994`/main Harbor run `39955701575` 均实际 success。Mega 完成数 10/110（活动 10/109），Libra 仍 0/26（活动 0/25），联合 10/136（活动 10/134）。下一前沿为 MTR-97（wire 解压执行资源准入）；FIX06 move、47 rebind、Native intermediate 三项原风险继续保留。旧 FAIL/STOP/取消/预算与缺失原件按各自时点保留。

> **最新事实前沿（2026-10-23 MTR-95 实际完成）：** MTR-95 已 done/complete：内部首次发布 API（`publish_scope`/`publish_scope_in_txn`、`ExpectedOldTip`、`publish_first_scope`）落地，8AC/3VER 核销；ER-05 经 8 轮收敛为真实 Claude/隔离 Codex 双 `VERDICT: PASS`；bump 0.42.64 后完整 C 为 20 targets、3020 passed/0 failed/3 原有 ignored，fmt/clippy/双 build 全 exit0；提交 `11e0527`、tag `v0.42.64`、人工 Release 与 tag Docker run `39938986411`/main Harbor run `39939027979` 均实际 success。Mega 完成数 9/110（活动 9/109），Libra 仍 0/26（活动 0/25），联合 9/136（活动 9/134）。下一前沿为 MTR-96（PACK 结构到原始对象解码）；FIX06 move、47 rebind、Native intermediate 三项原风险继续保留，未被本卡关闭。旧 FAIL/STOP/取消/预算与缺失原件按各自时点保留。

> **最新事实前沿（2026-10-23 MTR-95 ER-03 消费前置已处置）：** FIX-MTR-06 `v0.42.63` 已 done/complete（8AC/8VER、A 4/13/4/7、ER-05 双PASS、20 targets/3014 passed/0 failed/3 ignored、Docker `39910333195`/Harbor `39910285924` success）。root 已按 ER-03 对 MTR-95 的 06 move 补偿、47 rebind、Native intermediate 三项风险作「可核的不消费接口」处置：首次发布在自身事务内取 native root `FOR SHARE` 并复核 target active/path/generation，缺 bytes 拒绝且不写 membership。该处置不关闭三项原风险，也不改变 95 的 8AC/3VER、I/R、independent/self 边界；MTR-95 现可开工。Libra 仍 0/26（LTR-01 待 H1）。旧 FAIL/STOP/取消/预算与缺失原件按各自时点保留。

> **最新事实前沿（2026-10-23 FIX-MTR-06 实际完成）：** FIX-MTR-06 已 done/complete：8AC/8VER 核销，focused A 4/13/4/7，ER-05 真实 Claude/隔离 Codex 双 `VERDICT: PASS`，bump 0.42.63 后完整 C 为 20 targets、3014 passed/0 failed/3 原有 ignored，fmt/clippy/双 build 全 exit0；提交 `a69c408`、tag `v0.42.63`、人工 Release 与 tag Docker run `39910333195`/main Harbor run `39910285924` 均实际 success。Mega 完成数 8/110（活动 8/109），Libra 仍 0/26（活动 0/25），联合 8/136（活动 8/134）。下一前沿 MTR-95 仍须 root 按 ER-03 处置 06 move/47 rebind/Native intermediate 具名消费前置后才可开工；LTR 链仍待 H1。旧 FAIL/STOP/取消/预算与缺失原件按各自时点保留。

> **最新事实前沿（2026-10-23 FIX-MTR-06 完整C通过，待发布/D）：** FIX-MTR-06 在 `v0.42.63` 最终树上实际通过 focused A（4/13/4/7）与完整 C：20 targets、3014 passed、0 failed、3 原有 ignored，fmt/clippy/双 build 全 exit0；ER-05 真实 Claude 与隔离 Codex 双 `VERDICT: PASS`（P0/P1 none，P2 由 root 具名接受）。8AC/8VER 全部核销，Acceptance=`remote-pending`；精确 signed+DCO 提交、main push、标注 tag `v0.42.63`、人工 Release 与 Docker/Harbor D 随后按实际结果登记。Libra 仍 0/26（活动 0/25）；FIX09 已 done/complete，Mega 完成数在 FIX06 D 成功前保持 7/110（活动 7/109），联合 7/136（活动 7/134）。MTR-95 消费前置（06 move/47 rebind/Native intermediate）仍须 ER-03 审定，旧 FAIL/STOP/取消/预算与缺失原件按各自时点保留。

> **最新事实前沿（2026-10-23 FIX-MTR-09 实际完成；FIX-MTR-06 待新树完整C）：** FIX-MTR-09 已 done/complete：探针改为累计 deadline + 完整 `HTTP/1.x` 状态行，占用端口用例改为持有自身 listener 取端口；A/B 为 5 个新 socket 用例、`integration_api_write_trunk` 80/80×6、`integration_storage_events_runtime` 5/5×6；ER-05 R5 真实 Claude/隔离 Codex 双 `VERDICT: PASS`；bump 0.42.62 后完整 C 为 20 targets、3014 passed/0 failed/3 ignored，fmt/clippy/双 build 全 exit0；提交 `e2fbce4`、tag `v0.42.62`、人工 Release 与 tag Docker run `39904165417`/main Harbor run `39904120567` 均实际 success。Mega 完成数 7/110（活动 7/109），Libra 仍 0/26（活动 0/25），联合 7/136（活动 7/134）。下一前沿为 FIX-MTR-06：09 的 C/D 已闭合，须在其树上重新 bump 0.42.63 并重跑完整 C 后才可收口；FIX06 的 AC/VER 不因前次失败被改写。旧 FAIL/STOP/取消/预算与缺失原件按各自时点保留。

> **最新事实前沿（2026-10-23 FIX-MTR-09登记；FIX-MTR-06完整C受阻）：** FIX-MTR-06 最终树 `c7021c0` 的 A 组已实际复跑通过（VER-1/2/3/5=4/13/4/7，全 exit0），fmt、strict clippy、`cargo build`、`cargo build --tests` 均 exit0，ER-05 真实 Claude 与隔离 Codex 对同一冻结输入均 `VERDICT: PASS`。唯一未闭合的是 `source .env.test && cargo test --all`：`integration_api_write_trunk` 在 4 次真实运行中分别以不同用例在 seed clone 阶段 `Couldn't connect to server` 失败，失败用例的服务 stdout 仍停在 migration、`ss -ltnp sport=:<port>` 无监听者，根因为测试就绪探针把其它测试端口分配器短暂 `listen` 的端口当作服务就绪。按 ER-03 登记 `FIX-MTR-09`（测试 harness readiness 更正，独立 patch/self C-D，prod-files=0），FIX-MTR-06 保持 in-progress：须先取得 09 的 C/D，再以新树重跑 06 的完整 C 后才可 done/complete。计划总数 Mega 110（活动 109）、Libra 26（活动 25）、联合 136（活动 134），完成数仍为 Mega 6/110。旧 FAIL/STOP/取消/预算与缺失原件按各自时点保留；FIX06 的 AC/VER 不因该环境缺陷被标记完成。

> **最新事实前沿（2026-10-22 22:47:54 UTC为本轮事实正式写入起始秒，实际apply收据 `/tmp/fix06_A_smoke_final_fact_apply_actual_20261023/receipt.json`）：** FIX06批量生产实现及测试已应用，累计24个新增测试、当前批量来源新增13个；正式8AC/8VER及canonical限定不变。执行合同R3 actual70980/exit0/chunk7b76dc为真实Claude/Codex双PASS（含VER2_MAPPING），仅合同资格；新产品A actual13933/exit0/chunk7bb0e2十门真实通过：fmt/clippy/release各exit0，VER1/2/3/5分别4/13/4/7通过，VER6_7_capture另1通过，traces24，api433通过/1原ignored。84次native查询，32次清理信号发送有完整journal，remaining_owned={}。root已具名判定当前源码VER4/6/7/8手工证据PASS，VER2+VER5跨选择器映射按两门实际通过共同消费。新smoke执行合同86711/exit0/chunkcd70d8真实Claude/Codex双PASS；产品smoke81644/exit0/chunk7be561实际18通过、0失败，runtime OpenAPI HTTP200/3.1.0/30 paths，7个owned containers和10项fixture清理成功，host清理0信号/0错误、remaining_owned={}。本次事实同步B、自身ER05、完整C/D及提交/推送/版本发布仍未完成。 A与手工原件位置见Mega2 FIX-MTR-06 Current evidence；旧FAIL/STOP/缺失/取消和各历史pending原文保留，不能回填为当时通过。AC勾选、Lifecycle/Acceptance、Dependencies/I/R/独立发布边界不改；Libra13本地文档不提前push，MTR95仍独立阻塞。

> **执行事实更新（2026-10-22 22:01:45 UTC，本次写入时点）：** FIX06批量源码及测试已应用，实际产品A尚未启动。执行合同R1 session92123/exit1与R2 session24301/exit1均为Claude PASS、Codex FAIL；R2确认两项P1关闭，清理journal及独立观察连接P2正在前滚修复，未以评审次数耗尽停止。前两次准备实际64055/78353共扣72秒，原首个A claim未创建，后续准备继续累计；旧FAIL/STOP与FIX3/LTR00取消不变。VER2的owner预占零tree证据明确由VER5具名用例补充，必须两门真实通过；此映射澄清待下一轮双审，8AC/8VER、原命令/覆盖/期限、依赖与发布边界均不变。H0既成PASS仅为计划采纳；FIX06新A/自身ER05/C/D、提交与推送均未完成，Libra13笔本地文档不得提前push，MTR95独立阻塞保留。R2原件 `/tmp/fix06_batch_A_execution_review_r2_20261023/root-terminal.json` SHA `ca767f9eaaad960097ca08b1b42598276f80c7056c010dd48b837c8e3d6cedf0`。

> **最新结果登记（2026-10-22 21:26:53 UTC，root实际终态判定时点，非候选生成/未来写入时间）：** H0 R2真实session81653/exit0/chunkf7fd06，Claude与隔离Codex对H0_GOVERNANCE及FIX06_PLAN_ADOPTION均明确PASS；root已核同一manifest `b6d576d9af90cfbf678bf62f951ad58c798abfafeae3f95dd9df35b36dc1b181` 与全部冻结输入并完成P0/P1关闭、具名P2处置。仅准当前8AC/8VER+canonical计划采纳与H0前置闭合；FIX06批量源码、新A/B、自身ER05及完整C/D尚未完成。Libra52474来源资格仍仅限域carry（full10757/21ignored/1flaky、15focused及具名三门；network3；余1093），13笔本地文档不得提前push；MTR95的move/rebind/Native intermediate风险仍独立阻塞。R1 actual81582 ClaudeFAIL/CodexPASS、全部旧FAIL/STOP/missing/取消及审前pending保留为各自历史时点。root终态 `/tmp/fix06_joint_H0_final_review_r2_20261023/root-terminal.json` SHA256 `4338a0aac40b068a01663301c3a01d5d0d86ccaf333a804db7c98c92d404da28`；本登记不改变AC/Lifecycle/Dependencies或发布边界。

# mega2 长期移植规划（Mega → mega2 完全移植）

> **现行执行前沿（原8文档正式写入：2026-10-22 21:00:20 UTC；非本次新采纳或评审PASS时间）：Libra选定f966来源资格限域接受，FIX06的8AC/8VER与canonical采纳仍待本轮共同H0。** 新恢复方案84664/exit0/chunk8f6dc7真实Claude与隔离Codex双PASS后，actual52474/exit0/chunkac9169完成原network3通过/0ignored/0重试、42条命令全部exit0且owned清理为空；新各boundary源码inventory与VCS身份一致。仅具名继承旧fmt/strict clippy/product CLI build、15focused及full10757最终通过/21原ignored/1flaky；不把旧22040/31114 STOP、missing终态、迁移者UNKNOWN或非连续审计改写成通过。root终态 `/tmp/libra_f966_cwd_recovery_actual_after31114_20261023/root-terminal.json` SHA256 `3fb07e4b14bb915b6dbf1de6cbcb5e75f3f9656e33d94f972ea4f3456ad8c77d`绑定本地430b1cae/上游f966548/版本0.30.86。inner185.297950秒不是最终预算记账；outer188.596712秒加1秒launcher余量扣190，7200累计6107、剩余1093秒（不是1097），无自动新尝试授权。H0尚未PASS；FIX06批量实现、新A/B、自身ER05及完整C/D未完成，LTR卡未完成，13笔本地文档不得提前push。以下旧“当前/待资格/运行中”等段均按各自时点保留。 原实际写入由 `/tmp/fix06_H0_governance_apply_actual_20261023/receipt.json` SHA256 `6dd403e4bbd18ecbd6ba2c920d691dc18ef2e0a2533982aef6ef4b5bb56fcb0f`绑定；此前20:59:30为候选生成时间，H0 R1 actual81582/exit1/chunk061e55为Claude FAIL/Codex PASS，此次仅订正登记，仍待复审。


> **2026-10-23 更新执行前沿：Libra 31114启动前STOP，剩余1283秒，恢复与共同H0仍待。** actual31114/exit1/chunk899531因root从 `/media/eli/data/mega2` 启动，与冻结cwd `/media/eli/data/libra` 不符而在入口断言STOP；未启动native或产品命令，inner输出目录/claim/终态均不存在，不补造。外部一次launch已消费；实际观测32.762631秒及1秒launcher余量共扣34，原7200累计5917、剩余1283秒。root终态 `/tmp/libra_31114_cwd_stop_20261023/root-terminal.json` SHA256 `160ed68426886d37066f4e139eb1b6b3d34ddd0e2b5b29535e68968b09119f9b`不授权自动重试或新增窗口；修正启动cwd的新恢复仍待审及真实执行终态。以下22040的STOP、限定15focused/full10757最终通过/21ignored/1flaky及当时1317余额逐字保留为历史；network未取得资格通过。FIX06仍8AC/8VER及合法canonical存储状态等价/异常路径Err整txn回滚限定，批量实现未启动，当前采纳差异及两仓H0仍pending，不继承旧8/5设计PASS。无新增提交、push、tag、Release或CI/D；旧FAIL/STOP及FIX3/LTR00取消不变。


> **本次共同H0待审采纳差异：** 批量恢复仅对合法canonical存储状态声明静态等价。锁后inactive行须严格验证存储路径为canonical；异常路径返回Err并由外层事务rollback，不能silent normalize或顺手修DB。该防御归既有AC-5错误回滚与VER-2错误覆盖，不新增AC/VER，仍8AC/8VER；R2旧8/5设计PASS不覆盖本差异。


> **2026-10-23 当前执行前沿：Libra 22040实际STOP，FIX06采纳稿8AC/8VER待共同H0。** Libra actual22040/outer exit1/chunk9c321b：15focused全部exit0，full实际exit0、10757最终通过/21旧ignored/1flaky；随后固定0.30.69 CLI的rev-parse HEAD因数据库schema2026102201超过支持的2026101902而exit128，network未启动。full-after源码inventory与入场逐项相同，但不覆盖活仓VCS DB；原full-after-vcs及terminal-inputs缺失不可补造，迁移触发者UNKNOWN。整窗3214.211489秒扣3215，原7200累计5883、剩余1317秒；来源资格未完成、不构成H0/C/D或LTR完成。root仅登记1317余额内匹配0.30.86工具的新恢复方案，待新真实双审，未预授carry或执行PASS。FIX06方案R1 actual8843/exit1的Claude FAIL/Codex PASS保留；R2 actual15527/exit0/chunkbfc3c6真实双PASS仅覆盖旧8AC/5VER设计（manifest c1b9e7f8af1f6969819ec56e3c622303e8c19438ccad47e2a1cd4c8ccc1bec50、62输入核同）。当前采纳稿按G-03将SQL锁对象、唯一索引、fallible解析分列VER6/7/8，总8AC/8VER、无余量，尚待共同H0真实审查；批量实现、新A/B、自身ER05及完整C/D未启动。旧7/4与8/5证据和以下全部旧“当前/运行中”段按各自时点保留。Libra13笔本地文档仍不得提前push；Mega6/109、Libra0/26不变。


> **2026-10-23 FIX06同轴批量恢复v2待复审终态：** 当前候选新增AC-8/VER-5，粒度为8AC/5VER、四生产文件/三落点/M，行为轴、Dependencies、I/R及independent patch/self发布边界不变。拟将逐目标SQL改为排序分块锁定、分块owner查询、按层批取tree和受唯一约束保护的分块激活；批量实现尚未启动，新A/B、自身ER05和完整C/D均未执行。旧7AC/4VER输入的A10886与smoke78803只归历史源码，不覆盖未来批量树。方案层R1真实session8843/exit1/chunk68d34d为Claude FAIL/Codex PASS、整体FAIL，manifest `db813099268253c1998e032260e928f52a9d45dc2bd914734e27c78b7e9a0627`原件保留。v2以七项精确修订补齐跨chunk错误整txn回滚、owner四时点竞争、空ID仍验证root/Q1、root歧义、原无全表锁/无新增timeout重试、schema guard及borrowed txn计数正控制；新真实复审终态待，不预记PASS，也不以旧Performance文字澄清豁免GC-10。Libra22040仍在完整测试，最终资格和共同H0待实际终态审定。


> **2026-10-23 当前前滚控制恢复已双审、资格执行中：** 固定Libra来源430b1cae/f966548/0.30.86不变。56333控制STOP、已消费claim、0产品cases及缺失终态inventory继续保留，不补造旧终态。具名前滚控制恢复R1 actual37040/exit1为Claude PASS、Codex BLOCKED（越过可读allowlist），不是双PASS；read_guard修复后R2 actual10179/exit0/chunkcaf5d4真实双PASS，manifest `328049a5f61c4dfffad2c95b1cd9daf2baa03c3981986790041cd15304931d7b`，仅覆盖恢复执行安全及覆盖。准备2314/exit0耗7.012474秒扣8，原累计余额4540降为4532秒；新增时间0，审查轮次不重置旧测试/诊断预算。一次新执行22040已启动，当前没有实际终态，不能记产品资格通过或倒填最后退出、计数、清理及输入一致性。H0、FIX06自身/Performance/完整C/D、95独立风险及LTR01发布边界仍待；Libra13笔本地文档不提前push，Mega6/109、Libra0/26不变。下面56333段落是其原时点历史记录。


> **2026-10-23 56333时点历史资格STOP与恢复待审：** Libra固定整合来源 `430b1cae5a165adfc0f70858805bb480c085b737` / 上游 `f96654855d59455604b7b64669ba036b7b452ef7` / 0.30.86；13笔本地Traces文档重放、签名/DCO及上游产品树保留是既有整合事实，未提前push。listing恢复16996实际exit0，full10757 selected/21 ignored、network3/0，111条差异归属与6项未消费范围保留。最终执行合同R1为Claude PASS/Codex FAIL（binary privacy），R2 actual96103/exit0双PASS仅覆盖执行合同。产品资格 actual56333/exit1 STOP：53.788156秒扣54，账面余额4540秒，claim已消费，focused/full/network产品cases为0；缺失原terminal inventory，不能声称终态全树未漂移。确定的控制器缺陷是JSON list与runtime tuple比较误拒；两行修复及51项保存数据控制fixture已有原件，只是候选，不是产品/H0通过。root登记一次前滚恢复，仍从原7200累计余额扣账，旧claim/STOP不重置，须新真实双审与独立入口。H0、FIX06自身ER05/Performance及完整C/D、MTR95独立风险、LTR10的H1/WRITERS/LIVE和LTR01唯一发布边界均未解除；完成数Mega6/109、Libra0/26不变。以下旧“当前/待执行”均保留其原时点，不改旧失败、取消或预算。


> **2026-10-22 当前 Traces 前沿：** Libra 已将13个本地计划提交原生重放至最新上游 `f96654855d59455604b7b64669ba036b7b452ef7`（0.30.86），当前HEAD `430b1cae5a165adfc0f70858805bb480c085b737`；逐个DCO/签名及上游产品树保留已核，四份脏文档保留、index空、未推送。旧MC32四处格式候选已被上游吸收，旧R7只覆盖旧树；新整合源资格/当前治理双审待。完整复验按用户既有rebase重测授权具名执行，旧FAIL/STOP预算保留。FIX06自身/H0/C/D、MTR95独立风险及Libra H1/LTR01尚待，正式Mega6/109、Libra0/26不变。详见[日期计划当前执行前沿](plan-20261015.md#当前执行前沿libra-最新源整合2026-10-22)；以下旧“当前”按其原时点理解。

## 双评审迭代规则（2026-10-22 用户授权，当前生效）

用户原话：“请修改计划,双评审可以多次,直到双评审可以通过”。适用于 Mega2/Libra 两份 Traces 计划及其当前阻塞修复（含 FIX06、MC-32）的计划、源码和验收脚本双评审。root 可在处理本轮问题后继续发起后续轮次，直到真实 Claude 与独立上下文 Codex 对同一最终冻结输入均明确 `VERDICT: PASS`；不再设“双方各一次后必须重新申请”的总轮数限制，也不因上一轮 FAIL 或既有审查次数用完而重复询问这类评审授权。

每轮均保留独立原件、输入摘要、实际退出与终态；先修复问题或具名核清事实，再冻结新输入复审，不以重复请求施压取得 PASS。P0/P1 必须关闭，P2 按模板具名责任与条件处置。PASS 仅覆盖对应输入及审查范围；两人隔离审查，root 核实实际终态后才记双 PASS。每轮保留最多30分钟的进程看护上限；该上限不作为后续修复评审轮次的总预算。provider/MCP 等技术故障仍按 ER-10 分类及有界退避，真实凭据、额度或服务不可用须报告，不能伪造成功或静默空转。

本条只前滚调整双评审的轮次授权；历史 R1/R2/R3 FAIL、STOP、截止和原件原样保留，其“各一次/后续须另授权”只描述当时规则，不再限制本条生效后的审查。产品诊断、A/B/C 测试次数与窗口、旧 FIX-MTR-03/LTR-00 预算、验收标准、提交签名及发布边界仍分别按原任务卡执行；本条不授予新的产品测试次数或提前发布权限。解析器fixture通过不替代产品资格，当前修复版正式双审可直接按本条开展。

## 当前双审迭代：R6 终态与 v11/v12 修复（2026-10-22）

R6 实际 session24950 / exit1，Claude PASS、Codex FAIL；原件 `/tmp/libra_mc32_recovery_review_r6_20261022/`，manifest `e7cc6f3f5047901490c4cce45d9bdfaa0e6c871e7e43c59d5fc5ea387abc7a3f`。两方认可多轮评审规则及v10 flaky/leaky可观察边界；新的 `P1-R6-OUTCOME-METADATA` 是JUnit outcome只核非空type、console对账只核成功/失败，可能接受执行错误、普通失败、超时或recap类别矛盾。v11候选按固定producer严格核tag/type组合和每attempt的共同可观察类别；失败case主outcome是第1attempt、rerun按序，flaky事件对应成功前失败；recap对console规范化标签一致，不猜JUnit未给出的信号号码。未知type、不兼容tag/type、类别错配及recap矛盾均STOP。首轮纯解析671/671、exit0原件保留（chunk bd87a0，`/tmp/libra_mc32_junit_v11_parser_validation_actual_20261022/`）；随后补齐已知LEAK-FAIL但summary省略附计数的拒绝，最终纯解析676/676、exit0（chunk474cc7，原件 `/tmp/libra_mc32_junit_v11_parser_validation_r2_actual_20261022/`），最终候选run SHA `ae6c8fa177dbef0ad8c0c92f500057a9a6b18b964e5785a16f83407f45cc7e11`；原397名称保留，其中`final-error`错误接受预期改为拒绝，旧helper错误type及无JUnit stub输入修正另列清单，原397历史原件不改。五组旧v10错误接受/新v11拒绝对照成立；这是解析检查，尚待最终冻结后的真实双审，不是产品资格。

v12随后仅收紧本机Linux普通失败退出码为1..255：executor先把0归成功，排除信号后由Rust/libc WEXITSTATUS取低8位；无效0/负数/>255不能以普通failure type通过。独立LEAK-FAIL的error文本code0语义保持。实际纯解析760/760、exit0（chunk52149f，原件 `/tmp/libra_mc32_junit_v12_parser_validation_actual_20261022/`），原676项同名同判定、15组v11错误接受/v12拒绝；最终候选run SHA `97a28b7004f128b250060442dcb2fa9ff840fbf2bbe692e25be4b416be63f0c5`，待R7真实双审。

R6 P2具名处置（owner=root）：finalize前核产品授权原话及hash与多轮评审授权不同；产品启动前特别检查冻结`.pi`临时文件对应写者，记录不可见进程及点时观察限制，不停止用户写者、不豁免inventory；若实际hardstop exit124且缺terminal，外层仍记录真实退出、清理状态与已消费claim。继承R5允许新增selected全部执行、零删除/ignored变化，GC-MC-07五exact具名退出/集合/日志收据，execfail不一致STOP且不自动重跑。后续评审仍按用户持续授权，不另申请轮次。历史FAIL/STOP不改；FIX06自身/H0、MC32产品资格与C/D仍待，无新增产品运行、提交、推送或发布。下方旧“当前”均为原时点记录。

## 当前双审迭代：R5 终态与 v10 修复（2026-10-22）

R5 实际 session41138 / exit1，Claude PASS、Codex FAIL；原件 `/tmp/libra_mc32_recovery_review_r5_20261022/`，manifest `0f2f67cf5bffbcdb9644ae105f6fdb23db24913f55d5939f85cb4e2324da014c`。v9信号映射已受核；本轮 `P1-R5-FLAKY-LEAK` 是nextest重试成功即使泄漏也只打印 `TRY n PASS`、summary却同时计入flaky/leaky，旧脚本误拒合法结果。v10按固定nextest源码核对显式成功泄漏数K、无显式泄漏身份的成功flaky候选数F与实际summary泄漏数L，要求K≤L≤K+F，持久记录真实L、已知身份、候选集合和差值；不臆造具体flaky用例的泄漏身份。该日志/JUnit不能独立证伪区间内少报，明确保留此信息限制，其它逐case/summary/exit一致性仍严格检查。实际纯解析397/397、exit0（chunk d50c3f，原件 `/tmp/libra_mc32_junit_v10_parser_validation_actual_20261022/`），原363项同名同判定，三组v9 RED/v10 GREEN保留；最终修复仍待真实双审，不是产品资格。

R5 P2具名处置（owner=root）：相对旧资格基线的selected新增允许记录并全部执行，selected删除和ignored增删仍为零；本轮源码/config/env冻结，实际listing/JUnit身份精确一致，本条不减少覆盖。产品finalize须另绑定一次7200秒明确产品授权原话，禁止复用多轮评审授权；启动前只读检查`.pi`写者并记录不可见进程限制，不停止用户写者、不豁免输入。R4的GC-MC-07外层具名收据、五exact退出/集合/日志绑定和execfail不一致即STOP义务继承。继续双审无需另问次数；历史FAIL/STOP不改，FIX06自身/H0、MC32产品资格及C/D尚未完成。本轮无产品运行、提交、推送或发布。下方旧“当前”均为原时点记录。

## 当前双审迭代：R4 终态与 v9 修复（2026-10-22）

R4 实际 session78924 / exit1，Claude PASS、Codex FAIL；原件 `/tmp/libra_mc32_recovery_review_r4_20261022/`，manifest `08c59cc99bd88df39d9b6db28ba6f91d6238c8156534ff35fcd5c601c1bd14d3`。两方认可本页多轮双评审规则，R3捕获输出问题已在v8受核；整体仍FAIL，因为 `P1-R4-SIGNAL-STATUS`：pinned nextest 的重试信号缩写（如SEGV/KILL）会被旧解析拒绝。root按既有用户授权修复并继续下一轮，不另申请评审次数。v9候选精确补齐该版本信号/短状态映射，首轮纯解析实际363/363、exit0（chunk dbc0be，原件 `/tmp/libra_mc32_junit_v9_parser_validation_actual_20261022/`），原139项保留；候选最终输入与后续真实双审另行绑定，未记产品资格或P1最终闭合。

R4 P2具名处置（owner=root）：① finalize前须绑定与“多轮双评审”不同、明确授予一次7200秒产品运行的用户原话，并在外层收据引用；② 本次补齐MC-32 R2/R3/v8/用户授权/R4修订历史；③ 启动产品验收前只读核查`.pi`写者，不停止用户写者、不豁免冻结输入；④ execfail显示内容与JUnit描述不一致时记“full failed、metadata STOP、network未收集”，不自动重跑；⑤ 最终外层收据须具名GC-MC-07等价执行方式，并绑定五条原exact实际退出、结果集合、日志摘要。全部旧FAIL/STOP及产品预算保留；本次没有产品运行、提交、推送或发布，FIX06自身/H0与MC32 C/D仍未完成。下方旧轮次状态是原时点记录。

## MC-32 恢复审查 R3 终态（2026-10-22）

Libra MC-32 恢复合同实际双审 session8661 / exit1：Claude PASS、Codex FAIL；root采纳 `P1-R3-LOG-FRAMING`，合法测试输出被误判为nextest控制记录，须先修验收脚本。原件 `/tmp/libra_mc32_recovery_review_r3_20261022/`，manifest SHA256 `2a08cc88555a51c7a76cdb027563203caa1cf4964947c506e03897ac1034bd5e`。本次review-only各一次已用并STOP，未创建后继产品claim、未获新的7200秒产品窗口；旧R1/R2/R3失败与预算保留。v8修复候选已保存，纯解析实际139/139、exit0（chunk21281a；原件 `/tmp/libra_mc32_junit_v8_parser_validation_r2_actual_20261022/`），保留原81项及三项旧v7 RED/新v8 GREEN；候选尚待新双审，不当作产品资格或P1正式闭合。Libra四处格式修复已落工作区；FIX06已有A/smoke结果不变，自身最终双审、两仓H0同步、版本后完整C/D仍待。资格真实终态前维持既有H0同步顺序，完成数Mega6/109、Libra0/26不变；本轮无提交、推送或发布。下方较早“当前”段落均为原时点快照。

## FIX06 A 终态索引（2026-10-22；待审候选）

FIX06实际10886八门A exit0，仍in-progress/Acceptance空；smoke78803实际exit0/18通过、OpenAPI200，root配对终态已核、待最终冻结；文档B、自身/联合ER05及完整C/D仍待，smoke合同12055双PASS不代源码验收。97580编译FAIL、5349parserSTOP及原A绝对预算保留；逐八门signals0，14个native status查询共28发送/14身份，顶层仅最后2次；后代exe/cmd未保存，不能混称全程零信号或猜测程序归属。Mega6/109、Libra0/26不变。原件 `/tmp/fix06_A_parser_recovery_actual_20261022/`；详见[日期计划](plan-20261015.md)本轮入口。当前SSOT b58da1a新字节仍待最终审查及FIX06/LTR01各自覆盖；Libra资格/branch-force/H1/LTR01与95独立风险不因A通过解除。下方旧“当前”段落按其时点保留。

## FIX06实现语义绑定候选（2026-10-22；验收/最终审查待）

FIX06新增SSOT实现说明候选 `b58da1a818465da51b72d3148a91aad763d5317ec92c00efc13d5320cc51c07a` 待最终自身/H0联合双审及FIX06 C/D；Libra文档覆盖仍LTR01，MC32资格终态前不应用本Libra索引候选。完成数Mega6/109、Libra0/26不变；[权威事务合同及剩余边界](plan-20261015.md)。

## 当前执行前沿（2026-10-22）

FIX06/H0治理真实R3双PASS，ER01/02/03已核，FIX06现in-progress/空；自身实现、验收、ER05及完整C/D待。Libra固定99资格session77621因MC-32四处fmt差异实际exit1，原一次claim已消费，其余门未运行；修复与新恢复合同待。最新远端观察fbdf5ee未整合，发布前fresh整合重测。Mega6/109、Libra0/26不变；95独立风险、H1/LTR01及Libra不push边界保留。详见[日期计划](plan-20261015.md)当前入口，旧FAIL/STOP与预算不改。

## 当前99固定整合树前沿（2026-10-21T18:10:31.027815+00:00；新联合治理待审）

MTR100完整C/D与后置收口已完成，Mega6/109、Libra0/26不变。Libra13笔签名文档与上游MCP三行/原dirty治理精确保留，HEAD `57fb4accac009e4f09a76e466a32ef239a09d354` / upstream99a92c1、版本.84；新五门首次资格7200一次/29必选未执行，旧f0入口90973 STOP、0测试/0claim保留。FIX06仍pending，原7AC/4VER/4生产/3落点/M及SSOT e274439不变，联合双审待；branch-force独立交付阻塞、95具名风险、H1/LTR01与不push边界保持。发布前fresh整合重测，后续远端观察不追溯取消固定树真实结果。详见[日期计划](plan-20261015.md)。

## 历史f0候选前沿（2026-10-21T17:45:12.316323+00:00；本轮治理待双审）

MTR100及后置七文档已实际双审/提交，Mega main为9149149；.60完整C/D保持done/complete。Libra13笔文档已精确重放到f0ea585，本地HEAD f733ba7d83e84b191c244c75d8988ae2a1461738，新.83全量资格待、未push。FIX06按ER03待审4生产文件/3落点/M及六P2处置，新SSOT e274439未发布；7AC/4VER/独立patch不变，95其余具名风险仍阻塞。完成数Mega6/109、Libra0/26；历史失败/取消/STOP/预算不变。此次治理由FIX06与LTR01分别承载，不能继承旧.60覆盖新字节。完整事实与单次资格合同见[日期计划](plan-20261015.md)。

## 当前实际发布前沿（2026-10-21T16:49:06.220390+00:00；新增文档待提交）

MTR100源码、A/B、自身与最终双审、0.42.60完整C、签名提交、main/tag/人工Release及适用D已实际闭合：done/complete。Mega6/109（活动6/108）、Libra0/26（活动0/25），联合6/135（活动6/133）。DEBT-MTR-01仅disabled HTTP94+SSH100拒绝边界closed；enabled/H1/LTR01仍待。 下一前沿FIX06仍需ER01/02/03；MTR39等13，93来源限制与既有P2不变。Libra.83资格待、仅本地无push。本次文档新B/双审/精确收口另待。详见[Mega实际.60发布与D原件](plan-20261015.md)。


## 历史当前本地C前沿（2026-10-21T16:07:04.807973+00:00；提交/发布/D待）

MTR100源码、恢复树九门A/18项smoke、自身ER05及0.42.60本地完整五门C已核；仍in-progress / locally-accepted，最终交付双审、精确签名提交、main/tag/人工Release与适用D待。完成数仍Mega5/109（活动5/108）、Libra0/26（活动0/25）。 真实C session46316 exit0、20目标2985通过/3原有ignored；本次新增事实B与最终交付双审另待。Libra.83资格/H1/LTR01、DEBT-MTR-01仍未闭合。详见[Mega最终本地C原件](plan-20261015.md)。


## 历史当前版本入口前沿（2026-10-21T15:08:59.857205+00:00；完整C/D待）

MTR100恢复树A/smoke与原后置文档B已核，自身ER05 R2真实双PASS；0.42.59→0.42.60版本入口已实际完成，现in-progress / locally-accepted。0.42.60完整C、精确签名提交、推送/tag/Release及适用D仍待；完成数保持Mega5/109、Libra0/26。 实际生产落点仅ssh.rs，server.rs仅I允许未改；原523链接B已PASS，本次新事实B另待。R1 Claude PASS/Codex BLOCKED与前驱补件保留；Libra.83资格/H1/LTR01仍待、无push。详见[Mega自身审查与版本入口原件](plan-20261015.md)。


## 历史当前实际A前沿（2026-10-21T14:17:51.396714+00:00；自身ER05与C/D待）

FIX08已done/complete；Mega完成5/109（活动5/108），Libra0/26（活动0/25）。MTR100恢复后的新组合树九门A及完整18项smoke/运行时OpenAPI已据实际终态核验，现in-progress / locally-accepted；自身ER05、0.42.60完整C、提交/推送/Release与适用D均待。 Libra0.30.83新树资格仍待，H1/LTR01仍阻塞、未push；当前SSOT18327为未发布WIP。readiness R1的Claude PASS/Codex FAIL与R2双PASS均保留，新事实七文档静态B待；详见[Mega实际A证据与剩余边界](plan-20261015.md)，本索引不复制执行合同。


## 历史当前恢复前沿（2026-10-21T13:10:22.615922+00:00；待审）

FIX08已done/complete，Mega完成5/109（活动5/108）、Libra0/26（活动0/25）。MTR100六非Cargo路径已恢复，现in-progress；locally-accepted仅保留历史A/B输入资格，新组合树九门A及完整18项smoke未执行，新自身Review与0.42.60完整C/D待。 Mega HEAD/main6361830；restore6实际88081 exit0，SSOT18327为未发布WIP。Libra本地 `0dd5b4e842b59eb91588a399a23a523097cd22a7` 已整合 `27a72b3a1acc0dbb765b100995c706b76966f8ba`、实际 `0.30.83` 资格待、未push，H1/LTR01仍阻塞。9A拟7200、smoke另拟总7200（内含1800清理）、.60 C独立既登记7200；旧工具链原件不足不carry，旧FAIL/STOP/claim保留。详见[Mega恢复入口与证据合同](plan-20261015.md)；A与smoke同一readiness双审待，新A/Review/C/D未取得。


## 历史FIX08实际发布与D收口（2026-10-21T12:17:08.506230+00:00）

FIX08已完成源码、A/B、真实ownER05与最终交付审查、本地五门C、精确签名+DCO提交、main/tag/人工Release及适用D：done/complete。Mega正式完成5/109（活动5/108）、Libra仍0/26（活动0/25）。MTR100仅其FIX08发布前驱解除，可按受保护前像精确恢复六个非Cargo路径；恢复尚未在本收据执行，MTR100自身输入/Review/0.42.60完整C/D仍待，不能据此标本卡完成。

Mega signed commit `42137d6dbcffe9e906bb325bd17a8ae7846bc65c`，父 `c52bf4a99486e9f766192b5bb69a020d8ce83165`；Libra精确文档signed commit `076c24ab662ee88b12dde5c9cc905243c90a0acf`仅本地、不push，且不是新上游产品资格。两者实际GPG VALIDSIG/DCO及root工具终态均已核。main实际ref等Mega commit；标注 `v0.42.59` 对象 `973297f03a0a1c675531e959566c5af9c5474db3` peeled等同提交；人工Release https://github.com/gitmono-dev/mega2/releases/tag/v0.42.59。

Docker run 39595847652：两个原生架构job及manifest、所有关键发布step实际success；main Harbor run 39595506466：实际success。D实际root终态及完整API/ZIP原件由本次生成manifest逐项绑定，生成器不发起网络查询或workflow。

最终交付审查manifest `77e6b385c47de991648e1dc654a3f5db341dc494f96c1bb13d2dd54f7d1ee94b`，真实两方单项PASS；本卡0.42.59本地五门20目标2981/0/3既有ignored已核。此次七份收口文字是发布后的事实增量，仍待root静态B/必要真实审查与精确文档收口提交；不倒记为已包含在上述发布commit中，不新bump/tag/Release，也不扩大后续卡发布权限。

MTR100恢复只能六非Cargo路径，旧59失败版本前像保留不套回；Cargo须后续独立前滚60。本收据不运行恢复。H1/LTR01、来源受限条件及其他前驱仍按卡执行；Libra不提前发布。D1/历史C失败、旧C2未知、STOP/已消费claim/取消记录均保留，FIX08真实socket修复与本轮成功不补认历史41555根因。


## 历史FIX08最终本地C实际收据（2026-10-21T11:31:34.696631+00:00）

FIX08源码、六门A/B及ownER05双PASS已核；0.42.59最终五门本地C实际通过（session68149 exit0，20目标2981通过/0失败/3原有ignored/0过滤/0measured，fmt/strict clippy及双build零warning/error）。Lifecycle=in-progress、Acceptance=locally-accepted；尚未提交/推送/tag/人工Release/D，不能done/complete。MTR100 WIP仍受保护；Libra仍待新上游资格/H1/LTR01。

最终C合同审查实际session80476 exit0，Claude/Codex各一项PASS；manifest SHA256 `cbea41bbfa0dd265cf65ee8c30804edbf99533f008ebe9b7d6b6234a7179db90`。ownER05原件另在 `/tmp/fix_mtr08_er05_review_r1_20261021`（manifest `8c4d9c34971af3a832f9774202543a04651ec36f5ee313cc63c906f27d1a14ee`），不把本地C当提交/远端D。C receipt SHA256 `a01444d82f0a1ef41d9f387e9bb83cea6324c5d35f82c5ec54e76128d915ce6f`，root实际工具终态SHA256 `65e90fda2da4646203fa510bc85cafa1bc9a220edac14bc37f41ad798230bbb3`。

| 门 | 原命令 | 实际子exit | 原日志SHA256 |
|---|---|---|---|
| fmt | `cargo +nightly fmt --all --check` | 0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| clippy | `cargo clippy --all-targets --all-features -- -D warnings` | 0 | `320e25669939ab3661e08d5109c8067ce0675703b776974c6b29a4df02751c36` |
| full-test | `source .env.test && cargo test --all` | 0 | `d7439bba06b744d1cc9d0aa770eeaf2c3e5798a219da5ac5771e0b3989fac956` |
| build | `cargo build` | 0 | `1e320dfea6b333a03198a10b4d055b739e9cf966042f51d69dc45d0a7e3c1bd5` |
| build-tests | `cargo build --tests` | 0 | `0d6adee6320c697164cf8b91c1b425a33f222f8158fe9af6aadf0b46db70287c` |

完整目标逐case与MTR94 C4原日志闭包一致，仅新增两项本卡harness测试；三个既有ignored逐名核同，未跳过/弱化/增加超时或自动重跑。最终输入及各门边界一致，owned host清理为空；不声明共享daemon全局恢复。当前SSOT仍是已发布94的7794字节，18327 SSH说明仍仅受保护MTR100 WIP。

待精确signed+DCO提交、实际签名验证、fresh远端ref核验、main/tag/人工Release及适用CI/D全部终态；此前不放行MTR100 WIP恢复或0.42.60新C。Mega完成数仍4/109、Libra0/26。D1入口STOP/已消费claim、MTR100初始C失败及全部旧取消/FAIL/STOP保留；本卡结果不证明41555或旧C2根因。


## 历史FIX08 ownER05与版本入口实际收据（2026-10-21T10:39:42.328334+00:00）

FIX08 in-progress/locally-accepted；2AC/3VER及六门A/B已核，自身ER05真实Claude/Codex双PASS（session82523 exit0）；版本入口实际session25405 exit0，58→59两面一致。完整C尚未启动；签名提交、推送/tag/Release/D均待。MTR100 WIP继续受保护，未恢复；Mega完成仍4/109、Libra0/26。

ownER05 manifest SHA256 `8c4d9c34971af3a832f9774202543a04651ec36f5ee313cc63c906f27d1a14ee`；claude raw `c74a1d27802a24bd99378e890d9986ef32c162e3247a23e3b9812a7d943dd0b7` / terminal `dec58bc8399bec6224fce204c7ce3b1d07828887d132ff1bcce9d4507f01b431`；codex raw `8d4ef2faa7f267e32ec9e97f620aacd1e0f07071f3ddc25625d2a62f83a3334b` / terminal `802fc22ca1fb3335d68b8098d977ca749c413e9fc48197759acda96c7a7c778b`。两个review实际exit0/单PASS，P0/P1=0，root已具名接受P2。此审查不代后置版本最终C，旧治理/AB/FAIL/STOP原件保留。

版本入口原件 `/tmp/fix_mtr08_version_entry_actual_20261021`；receipt SHA256 `b1f713eddcdbe002818b0ca015199b49d25f9ea1e6e1e192f5a4ece73391ffd2`，root terminal SHA256 `df9dad6433f1d801115f91db177ef1a28aac0f337501ad213040834b6e67fc3f`。`cargo check -p mega2`与`cargo metadata --format-version 1 --no-deps --locked`实际均exit0，只有Cargo.toml/Cargo.lock自身版本58→59；受保护MTR100原59失败树未重用。入口owned host清理为空，不声明共享daemon恢复。该入口不是五门C或build验收。

当前SSOT `7794d82f6a34eb7741fc2f17eeeb6789464db29a405c506ffdfe36544d0fb4b4` 是已发布94；`18327a4f3277f028350ab03c231570261ae54785e8c5900816a1b5a6f4cb8fec` 仅为受保护SSH WIP。D1入口STOP/已消费claim、MTR100初始C失败、全部旧取消与预算保持，不认定41555或旧C2历史原因。

Codex P2：本轮修正FIX08测试矩阵陈旧pending文字，保留旧行快照；最终C输入还须绑定原C4 raw log与实际target推导原件。root须完成本次后置文档静态B并冻结最终C输入；完整C/D实际通过后才恢复MTR100六非Cargo路径并独立0.42.60发布，Libra仍待自身新上游资格/H1/LTR01。


## 历史FIX08 A/B终态与治理采纳收据（2026-10-21T10:23:18.538031+00:00）

FIX08六门A/B原件已核（2026-10-21T10:23:18.538031+00:00）：in-progress/locally-accepted，2AC/3VER已核；ownER05、版本bump、完整C、签名提交、推送/tag/Release/D均未执行。MTR100 blocked/locally-accepted，受保护WIP尚未恢复；完成计数仍Mega4/109（活动4/108）、Libra0/26（活动0/25）。

治理R1真实Claude/Codex各PASS，root实际session59044 exit0；manifest SHA256 `f72e00e3f59f865ca01e236a8a57cd5a5afd1fdb32af2749aeb7566a0607a517`。此为治理/执行方案审查，非本卡实现后的ownER05。D1入口STOP/claim及MTR100初始完整C失败保持，不认定41555或旧C2原因。

当前SSOT为已发布94字节 `7794d82f6a34eb7741fc2f17eeeb6789464db29a405c506ffdfe36544d0fb4b4`；SSH disabled说明 `18327a4f3277f028350ab03c231570261ae54785e8c5900816a1b5a6f4cb8fec` 仅在受保护MTR100 WIP，未恢复/未发布。FIX08仅测试harness，无生产/协议语义变化。

| 门 | 实际命令 | 子进程exit | 原日志实际计数 | 日志SHA256 | 外部工具终态 |
|---|---|---|---|---|---|
| red | `source .env.test && cargo test -p mega2 --test integration_api_write_trunk service_readiness_rejects_tcp_self_connection -- --exact --test-threads=1` | 101 | `{"failed": 1, "filtered": 74, "ignored": 0, "measured": 0, "passed": 0}` | `4a6e7b17b04a74c679b34ada27ed904f258883fc77d46b158675977d05e4cfc8` | root session59654 exit0 |
| green | `source .env.test && cargo test -p mega2 --test integration_api_write_trunk service_readiness_rejects_tcp_self_connection -- --exact --test-threads=1` | 0 | `{"failed": 0, "filtered": 74, "ignored": 0, "measured": 0, "passed": 1}` | `4dbe4409231cdc8a62f7c60b88a0da53f2c5b022b340b6d20204c5d071509f7f` | root session79419 exit0 |
| positive | `source .env.test && cargo test -p mega2 --test integration_api_write_trunk service_readiness_accepts_real_listener_connection -- --exact --test-threads=1` | 0 | `{"failed": 0, "filtered": 74, "ignored": 0, "measured": 0, "passed": 1}` | `0736f132568916edc04c9fc4fbe085a4d8945fe7881ae318427d6b1272592671` | root session17258 exit0 |
| target | `source .env.test && cargo test -p mega2 --test integration_api_write_trunk` | 0 | `{"failed": 0, "filtered": 0, "ignored": 0, "measured": 0, "passed": 75}` | `2a47afe7f08c39648c9f7de670495ffe6f3d81893186cb24fc44a458d68a2013` | root session7847 exit0 |
| fmt | `cargo +nightly fmt --all --check` | 0 | `{"failed": 0, "filtered": 0, "ignored": 0, "measured": 0, "passed": 0}` | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` | root session42739 exit0 |
| clippy | `cargo clippy --all-targets --all-features -- -D warnings` | 0 | `{"failed": 0, "filtered": 0, "ignored": 0, "measured": 0, "passed": 0}` | `e2e61bfe5e5d676e24160850c50b58da817d7a45fd0125782f1e549eac1fe9da` | root session75657 exit0 |

RED exit101仅为AC1旧语义预期失败；GREEN、正常listener和完整原并发target分别有独立实际终态。完整target逐名对应原73+新增2，未过滤/未新增ignore；fmt与strict clippy零warning/error。受审输入前后同一、owned host清理为空，不声明共享daemon全局恢复。原始依据 `/tmp/fix_mtr08_ab_actual_20261021`，此次动态核验摘要见生成器输出manifest。

待root执行文档B及本卡实现真实Claude/隔离Codex ownER05；通过后才可在独立版本面上执行FIX08完整C/D。FIX08实际D前不恢复MTR100六非Cargo路径，不放行其0.42.60完整C，不提前Libra发布。


## 历史FIX-MTR-08采纳前候选（原冻结文字保留；后续实际状态见文首收据）

FIX08采纳候选待双审；Mega109/活动108、Libra26/活动25、联合135/活动133；完成仍4/109与0/26。D1入口STOP、未运行测试且claim已消费；MTR100 blocked/locally-accepted，独立FIX08完整C/D前不放行。

- **D1实际终态：** 原件`/tmp/mtr100_trunk_diagnostic_actual_20261021/{receipt,observer-errors,queries,actual-outer-tool-terminal}.json`；root实际session52522 exit1、28.303519521秒，claimed=true、launched=false、leader exit=null、0个服务。Libra `status --porcelain` 查询20秒TimeoutExpired触发入口STOP；原73例目标未启动，test_result=UNKNOWN、root_cause=UNPROVEN，不能记为产品测试FAIL、未复现、根因或完整C。D1 claim保留且不得重开。R3真实Claude/Codex各单PASS，actual session97178 exit0；原件及effective/root/tool terminal位于`/tmp/mtr100_diagnostic_plan_review_r3_20261021`，manifest SHA256 `363ba5317bd4253327ffc47b16c33c287d508cbca526f9f16f3183690b1d771a`，root实核3253审查项/3264取证项一致；P2具名处置已记录。此PASS只覆盖D1方案，随后入口STOP不追认为目标运行。
- **候选依赖：** `MTR-94 + DEP-TR-SERVICES → FIX-MTR-08 → MTR-100 → FIX-MTR-06 → MTR-95`；其后原宏链、H0～H6及LTR01唯一发布边界不变。FIX08只修测试TCP自连接假就绪，不补认41555、旧MTR94 C2或FIX3 C2历史原因。FIX08 pending/空、2AC/3VER、landing=1/prod-files=0/S；MTR100原5AC/1VER不削减，旧A/B/审查保留原输入限定。
- **WIP隔离实际入口：** fresh fetch actual26898 exit0，HEAD=origin/main=`c52bf4a99486e9f766192b5bb69a020d8ce83165`。8非plan路径独立WIP/HEAD原字节与SHA保存在`/tmp/fix_mtr08_wip_protection_20261021`；精确native stash actual96785 exit0，OID `cfacd14da3bf980ddf7c0f451e3632044a75e604`，保留不pop。隔离核验actual13225 exit0：883个非plan跟踪文件逐blob匹配发布HEAD，index空，工作区仅4计划，版本0.42.58；首轮只读核验因不支持refs/stash语法失败保留，改用原生JSON查询核明stash身份。现行SSOT SHA256 `7794d82f6a34eb7741fc2f17eeeb6789464db29a405c506ffdfe36544d0fb4b4`；`18327a4f3277f028350ab03c231570261ae54785e8c5900816a1b5a6f4cb8fec`仅为受保护MTR100 WIP历史输入，SSH改动不在本卡验收树。沿用原工作区，无shared-worktree/reset/force；实际harness尚未修改。
- **独立发布顺序（待审）：** FIX08 A/B（含旧语义确定性RED及同oracle GREEN）、自身真实双审后，亲自bump0.42.58→0.42.59并执行完整五门C、签名+DCO、本卡main/tag/人工Release及全部适用D；其D实际闭合后，仅从前像精确恢复MTR100六个非Cargo路径，保留stash，Cargo另按新基线前滚至0.42.60。MTR100必须重核输入/依赖/自身Review并独立完整C/D，不继承FIX08对其SSH WIP的覆盖。原MTR100失败树0.42.59和claim永久保留，不倒改为0.42.60。
- **新恢复授权与预算（治理待双审）：** 用户最新明确指令为“请分析 C 失败的原因并修复,然后重新启动完整的验收,继续推进计划完成”；root据此登记本次必要独立修复及后继验收，不复用此前一次120分钟claim。FIX08 A/B六门RED→GREEN→正常连接→完整trunk→nightly fmt→strict clippy，各仅一次，单门≤30分钟、总墙钟≤120分钟（含RED/GREEN切换）；源码修复后FIX08自身完整五门C一次≤120分钟，再在其C/D闭合及MTR100恢复/验收/审查后，MTR100自身完整五门C另一次≤120分钟，三者独立claim、首非预期失败STOP、无自动重试。两个完整C均保留原fmt/clippy/full/all-build覆盖与全部断言/原超时；本轮治理审查须核此分立范围。新A/B执行器及具体冻结清单另绑定本轮最终输入，C执行器须在各自最终版本树启动前再冻结双审；不借治理PASS省略自身ER05。
- **跨仓与证据承载：** Mega四计划为FIX08 I的附随治理；Libra三计划仅MTR01既有文档治理写集、C/D仍待LTR01，不允许Libra提前push。SSOT和两仓生产契约不改；Libra新上游资格待验、旧28来源卡blocked、53风险、取消验收和全部历史FAIL/STOP不变。本轮新增治理尚未双审，不借旧108/26或D1的PASS覆盖。新卡采纳、源码、验收、审查、提交、推送、CI/发布各阶段分别记账。


> **历史MTR100恢复R3待审（2026-10-21）：** D1方案R2为真实Claude FAIL/Codex PASS，actual session17407 exit1；v4已关闭旧三P1，另有adopted orphan误STOP和持有服务日志未copy两P1。v5保守处置已准备、待新双审；D1仍未执行/claim未创建，原完整C失败及已消费120分钟保留。当前一次30分钟完整trunk取证、其后ER03归属修复及新完整C授权有效；不减覆盖/断言/超时，不重开FIX3/LTR00旧预算。Mega源码未修，MTR100 blocked/locally-accepted；Libra已整合fc5至0c1613da、新上游资格待验；未提交/推送/CI发布，FIX06/H1/LTR01仍阻塞。

> **历史MTR100恢复R2待审（2026-10-21）：** 当前诊断方案R1真实Claude PASS/Codex FAIL，actual session9416 exit1；三P1为祖先身份、退出码归属及两仓冻结不完整。v4候选已修并待新真实双审；尚未执行D1、claim未创建，旧C失败与已消费120分钟不变。用户已授权分析/修复/新完整验收：D1仍仅一次30分钟完整trunk目标取证，原命令/覆盖/超时/并发不改；之后先独立归属修复及自身验收/Review，再新完整C。Mega c52bf4a、Libra已整合fc5至0c1613da，源码未修/新测试未启动；MTR100 blocked/locally-accepted，DEBT open、FIX06和H1/LTR01仍阻塞，未提交/推送/CI发布。

> **历史MTR100恢复R1待审（2026-10-21）：** 用户现授权分析当前C失败、修复并重新完整验收。初始C session21819 exit1/120分钟claim已消费；失败事实审查session24412真实Claude/Codex双PASS仅覆盖失败记账。现提出一次30分钟完整trunk目标取证，须新最终输入双审及实际终态后启动；根因尚未证明，未修源码、未重跑C。后续修复需ER03归属及自身验收/审查，再以新claim执行一次最多120分钟原五门完整C；不复用旧预算。MTR100仍blocked/locally-accepted、DEBT open、FIX06未开工；提交/推送/tag/Release/D均未执行。Libra已获取fc5eafc新上游，保留其Worktree/MCP成果及Traces本地历史，新上游源码资格待验，LTR01不提前发布。

> **历史MTR100完整C失败（2026-10-21T07:23:46.242313+00:00）：** MTR100初始完整C已实际FAIL/STOP：controller session21819 exit1，fmt/严格clippy exit0；原`source .env.test && cargo test --all` exit101，已到5目标合计2775通过/1失败/3原有ignored/0过滤，后续目标及双build未执行。失败为`delete_entry_reject_traversal_400`夹具seed阶段Git clone连接本地41555端口被拒绝，未到400业务断言，服务退出/端口归属原因尚未证明。一次120分钟claim已消费（1416.872秒），不自动重跑或复用余时；0.42.59已bump而未提交/推送/tag/Release/D。卡现blocked/locally-accepted，原A/B及R4源码/执行器双PASS保留其限定输入，不覆盖C失败。DEBT open、FIX06未开工，Libra仍本地e938c05，H1/LTR01边界不变。

> **历史MTR100入口资格（2026-10-21T06:24:32.425312+00:00）：** R3真实Claude/隔离Codex对源码/A/治理及v5控制器均两项PASS，actual session84473 exit0、541输入核同，原健康探针P1关闭。版本入口随后两次preflight STOP：SSH22 banner超时（443严格既有key认证与原生origin查询成功）及PG缺省HostConfig.Init直接取值报错；均未改版本、未消费entry/C claim。v6只修可选Init读取，真实三容器精简查询exit0，Git仍强制Init=true；待新R4双审后另用新入口路径。0.42.58、locally-accepted、完整C/提交/发布/D待和Libra仅本地边界不变。

> **历史MTR100控制器修订（2026-10-21T05:52:19.021295+00:00）：** MTR100源码/A及关联治理在R2仍获真实Claude、隔离Codex双PASS；C执行器v4为Claude FAIL/Codex PASS，root实际session31967 exit1，421输入核同。P1为Docker健康探针临时ExecID导致误STOP，禁止执行v4。root正在独立修订v5并安排新双审；用户继续测试授权有效，初始完整C一次120分钟尚未启动/未消费claim，版本仍0.42.58。5AC/1VER与locally-accepted保留，提交/推送/CI-D未执行，DEBT open；Libra只本地，旧失败/取消/预算不重开。

> **历史MTR100审查阶段（2026-10-21T05:35:07.494545+00:00）：** MTR100本地A/B保持通过；R1真实Claude与隔离Codex对自身实现/验收及两仓关联/C合同各给两项PASS，actual session52532 exit0、398输入终态同一，P0/P1=0。root具名接收5项P2并修订过期H0摘要与ignored标签；C执行器须另审。未执行v2控制器，静态修为v4：按本仓doctest=false允许空doc集合、只核本轮Running产物、只读VCS schema漂移门及精确Docker资格。新v4和本轮事实同步待R2真实双审；0.42.58仍未bump，完整C/提交/推送/D未执行，DEBT仍open，Libra仍仅本地。

> **历史MTR100本地验收（2026-10-21T05:23:55.973928+00:00）：** MTR100最终SSH禁用拒绝实现及A/B已通过：VER1 G3为2/2，完整Traces24/24，SSH29/29、HTTP8/8、server39/39、API433通过/0失败/1原有ignored；nightly fmt、严格clippy、fresh release build均exit0、零warning/error。新Docker Git/LFS原18项矩阵及运行时OpenAPI、owned清理已通过。现in-progress/locally-accepted，自身与两仓联合真实双审尚待；版本仍0.42.58，独立patch0.42.59及完整C/D未执行，未提交/推送本卡。RED、首轮夹具FAIL和clippy首轮FAIL保留；93/94 done及来源限94/100拒绝路径不变，Libra本地e938c05、H1/LTR01阻塞与不push边界不变，DEBT-MTR-01仍open。

> **历史MTR100阶段（2026-10-21T04:49 UTC）：** disabled SSH源码候选与专有VER1已真实2/2通过、0失败/ignored，807输入稳定，服务退出；首轮旧码RED与夹具URL编码FAIL保留。现继续fmt/clippy、SSH单元、完整Traces target、HTTP/server/API及原18项Git smoke；全A/B、自身双审、版本及完整C/D尚未取得，仍in-progress/空。93/94完整闭环、Libra仅本地且待H1/LTR01的边界不变。

> **历史MTR100候选复验状态（2026-10-21T04:43 UTC）：** MTR100旧源码RED已真实0通过/2失败（disabled仍广告capabilities），修复候选已落一生产文件；首次候选验证仍0通过/2失败，定位新增Git SSH URL正对照未转义字面%，传到服务端的是保留根而非预期普通路径。保留FAIL并只修夹具URL编码后重验，生产守卫/断言/超时不放宽；5AC/1VER尚未验收，自身双审与独立patch完整C/D待。MTR93/94 done/complete、Mega main c52bf4a/Libra本地e938c05和LTR01发布边界不变，旧FIX3/LTR00及MTR94 C4预算不重开。

> **历史开工状态（2026-10-21T04:23:33.853724+00:00）：** MTR94完整C/D及最终D事实双审已核；Mega文档收口c52bf4a已推main，Libra对应signed e938c05仅本地。93/94 done/complete，01仅Mega侧覆盖，Libra仍待LTR01；MTR100于2026-10-21T04:22:24.959231+00:00完成ER01/02/03，现in-progress/空，准备实现disabled SSH拒绝。5AC/1VER未验收、自身双审与独立patch完整C/D未执行；不开放enabled数据面，不重启旧FIX3/LTR00或C4窗口。

> **历史94收口发布终态（2026-10-21T04:02:51.712358+00:00）：** MTR94源码/A-B及自身真实双审、C4五门（2979通过/0失败/3原有ignored）、signed04c42e1、main/标注v0.42.58/人工Release已核；版本Docker run39539957757的amd64/arm64/manifest三job全部success，main Harbor run39539882313亦success。root于2026-10-21T04:00:24.847557+00:00重新取得原始API并核同提交/ref/Release。94现done/complete；93原signed2320e95精确树获94 C/D覆盖，done/complete，来源资格仍仅94/100拒绝路径；01本增量Mega覆盖已取得，Libra侧仍待LTR01，整卡不done。MTR100仍pending，须重新ER01/02/03后开工；本次D事实文档自身B/双审及精确提交待，不倒记为版本预发布字节。

> **历史94收口跨仓状态（2026-10-21T04:02:51.712358+00:00）：** Libra选定上游3b1200a/0.30.69，full10658/network3资格已核，21既有ignored、1 flaky、future-incompat警告保持；已签名本地文档checkpoint b157396f5c1aacbbaf5071b23ef34d9184f968e6（父9314a17），未push。H1/LIVE/WRITERS与Worktree冻结、LTR01唯一发布点不变。Mega正式完成4/108（活动4/107），Libra0/26（活动0/25）；其余旧blocked不批量改判。

> **历史D待定发布事实（2026-10-21T03:43:53.672324+00:00）：** MTR94源码/A-B、自身R2及C4后置最终真实Claude/Codex双PASS已核；C4五门exit0、编译0警告/错误，full2979通过/0失败/3原有ignored（20目标）。精确13路径signed+DCO提交`04c42e10ba88ed45b830d509407d5732f4f17845`已推main；标注`v0.42.58`及人工Release已核。版本Docker run39539957757与main Harbor run39539882313均已触发，最后观察运行中，D尚待；94为in-progress/remote-pending，93覆盖及100前驱未闭合，100未开工。

> **历史D待定跨仓事实（2026-10-21T03:43:53.672324+00:00）：** Libra选定上游`3b1200a80e00e8d1a8866ab07aa790c27f8a17a9`、0.30.69资格保留：full10658与network3实际PASS，21原有ignored、1 flaky及future-incompat警告不隐去；三份计划文档精确signed+DCO提交`9314a17348aceb0c631139715fd213ead73ed90c`已验签，仅本地未push。H1/LIVE/WRITERS与Worktree发布冻结、LTR01唯一发布点均不变，formal0/26。


> **历史提交前发布执行状态（2026-10-21T03:14:24.621527+00:00）：** MTR94源码/A-B及自身R2真实双PASS保留；新授权C4一次120分钟已实际执行，0.42.58最终源码树五门均exit0、编译0警告/0错误；full20目标2979PASS/0FAIL/3原有ignored、无过滤。窗口2082.320秒，98530输入终态同一，owned host为空/Git容器已停。当前in-progress/locally-accepted；后置文档最终双审、精确签名提交、main/tag/Release和实际D待，未填done/complete。原C2/C3 FAIL/STOP、消耗claim及历史C2原因未知保留；不复用剩余时间。

> **历史提交前跨仓事实（2026-10-21T03:14:24.621527+00:00）：** Libra选定上游3b1200a、本地2332823f1a2841a4e3121b164965dea25dea98fc/0.30.69；新树fmt/clippy/CLI build/full10658/network3实际exit0，21原有ignored、1 flaky与proc-macro-error2 future-incompat警告保留；后置三文档最终R2真实双PASS，signed+DCO2332823及精确3路径/签名已核，仅本地。此次两仓新事实文档另作B/双审；H1/LIVE/WRITERS与Worktree发布冻结不变，LTR01唯一发布点、formal0/26，未push。

> **历史本地验收与审查状态（2026-10-20T14:32:22.290875+00:00）：** MTR94的LFS原URI同轴修复已完成；新真实HTTP3/3、HTTP单元8/8、server39/39、API433通过/0失败/1既有ignored，fmt/严格clippy、release构建、完整Git/SSH/LFS18/18及运行时OpenAPI均通过。原LFS RED与R1 Claude PASS/Codex FAIL保留；修复后R2自身/联合双审尚待，完整bump树C/D未开始，未提交/push/tag/Release。93仅94/100来源资格、C/D94待；100未开工。

> **历史联合输入：** SSOT SHA256 `8160b5b9036d0ca7f6c240ea8d0f05294bdd453f0007d82a637a5b9ab9929210`，本次R2双审待；Libra已整合选定上游e4d843d至e2509aa，source与上游一致，四份本地文档patch重签/GPG/DCO核同；0.30.68新CLI已构建。原full仍以umask0002/单build job执行10636项，已有五项终次FAIL（原三次尝试保留），原network按既定命令续行，均不能称PASS。三项文件mode断言实际0775≠0755；标准umask022仅为待终态归属后的环境恢复候选，尚未改变或重跑。21skipped全部ignored且上游既有，具名审计保存。H1/LIVE未交付，LTR10/11/12与LTR01不跳前驱；Worktree发布冻结、COV/OG及旧取消/STOP不变。

> **历史修复进度（2026-10-20T14:05:07.114275+00:00）：** MTR94已实际复现LFS编码别名200绕过，原R1 FAIL保留；保存原URI的同轴修复已应用，nightly fmt/严格clippy已exit0，三项HTTP GREEN及其余表面门正在重验，R2双审与C/D仍待。SSOT当前候选 SHA256 `8160b5b9036d0ca7f6c240ea8d0f05294bdd453f0007d82a637a5b9ab9929210`，旧PASS不得覆盖新字节。Libra最新上游e4d843d已整合，当前e2509aa/0.30.68 CLI构建成功，全量与network正在独立复测；其三文档保持冻结至终态。未bump/提交/push/tag/Release。

> **历史审查阻断（2026-10-20T13:48:54.964226+00:00）：** MTR94首轮自身审查Claude PASS/Codex FAIL；LFS重写可丢失原保留定位符P1尚待修复/真实回归，禁止进入C。原A结果保留限定覆盖，AC4/5/6和VER1待补证；root已在原轴/I内完成ER03复核，未bump/commit/push。Libra新CLI0.30.68实际构建/version/SHA核验通过，新树full/network待。

> **历史验收登记（2026-10-20T13:29:58.843544+00:00）：** MTR94源码已实现；真实Docker Git三项HTTP门3/3、HTTP单元8/8、API433通过/0失败/1既有ignored、fmt/严格clippy、release构建及release产物完整Git smoke17/17均通过，运行时OpenAPI30 paths/HTTP200。A/B已取得，Acceptance=locally-accepted；自身Claude/Codex ER05尚待，未bump/提交/push/tag/Release/CI-D。MTR93的C/D94仍待，100未开工。Libra已整合e4d843d至e2509aa，四份文档重签/精确patch核验通过；fmt/clippy退出0，新树full/network与0.30.68 CLI构建待。旧55957全量在测试开始前受控中止，不记PASS。

> **历史源码契约输入（MTR94自身/联合审查待）：** `docs/refactoring/traces.md` SHA256 `4d4df6cd32ee5e476f6f1fc6cdfac3aad108fea9196c3cc945409da47f3dde2a`；仅同步HTTP disabled守卫实现与匹配边界，SSH100及enabled11/39/92未交付。旧771fb治理R2双PASS只绑定旧字节，不替代本次审查。

> **历史实现登记（2026-10-20T12:53:45.554412+00:00）：** MTR94已应用两生产文件HTTP禁用守卫，真实Docker Git夹具完成后旧实现discovery返回200/预期404，取得单case产品RED（`/tmp/mtr94_fixture_r6_20261020/receipt.json`）；完整三case GREEN正在运行，未取得本卡A/B或ER05 PASS，6AC/1VER仍未勾，未bump/commit/push/发布。先前编译与夹具失败均保留、未计产品RED或PASS。Libra四份本地文档提交已重放并重签至上游55957，HEAD `a51243785e10fb5b9feea5a4207fbc80213ad5bc`，整合全量验收进行中；Worktree冻结、H1等待及LTR01边界不变。93的限制与53项历史风险继续有效。

> **历史执行登记：** 2026-10-20 12:26:15 UTC记账：MTR93的17AC/17VER、文档B、自身真实Claude/Codex双PASS和精确signed+DCO交接已核；Mega `2320e95a80b5d813534d9ce8696baff1b7aab51f`、Libra `79f7cfaddbc72070a9b8d2a42c06410387c07fef`均仅本地未push。93为source-qualified（仅94/100拒绝路径）、in-progress/locally-accepted，C/D94待。MTR94于12:22:51 UTC完成ER01/02/03正式开工，in-progress/空，源码实现中；6AC/1VER全部未勾，验收/自身双审/版本及C/D均未执行。38/91 done/complete，01/93本增量Mega覆盖待94，Libra待LTR01；H1未交付，旧53风险/28卡blocked/取消及FAIL/STOP保持。总108/26、活动107/25。

> **MTR93历史审查时点（2026-10-20T12:21:45.754218+00:00）：** 17AC/17VER及文档B已PASS；本卡自身真实Claude/Codex均exit0、VERDICT: PASS，67冻结输入核同，P0/P1=0，root具名接受Claude五P2（见日期计划末尾）。Libra三文档事实跟进自身ER05亦双PASS。精确signed+DCO本地交接与验签仍待，核实后才开94；本轮只准94/100禁用拒绝路径，95及后续身份消费仍受47/06风险阻断。93保持in-progress/locally-accepted，C/D94待；LibraC/D仍LTR01，无push/tag/Release。下面较早时点的审查/B待属于先前阶段。

> **MTR93文档B终态（2026-10-20T12:12:48.467071+00:00）：** 134卡/223依赖边无环、467相对文件链接零缺失；两仓native diff check均exit0、暂存为空，只有已登记4+3文档变更。实际收据`/tmp/mtr93_start_20261020/B-receipt.json`；17项audit核验及本次B均PASS，自身真实双审和source签名交接待。以下较早时点的“B待”保留为先前阶段，不作当前阻塞；C/D仍待94/LTR01。

> **历史执行登记：** 2026-10-20 12:09:16 UTC：108/26治理H0与MTR01自身ER05双PASS，Mega `a077fe65dfe0dbd1becfe5432e28c871827dd9cb`、Libra `c2c76b3e1f926ebfaa8dd3d276b564adbbeecf3b`均已精确signed+DCO本地提交并密码学验签，未push。MTR93正式17AC/17VER核验通过，53项root处置逐条登记；现in-progress/locally-accepted（最终文档B已PASS），自身ER05/精确提交待，94/100受限资格尚未移交。38/91 done/complete；01及93本增量Mega C/D待94、Libra治理待LTR01。当前前沿93最终B已PASS→自身真实双审→签名交接，94未开工；H1仍待。总108/26、活动107/25；旧FAIL/STOP与28卡blocked保持。

> **历史治理终态（2026-10-20 11:57:33 UTC记账；审查终态11:49:57 UTC）：** 108/26治理H0与MTR-01本增量自身ER-05，真实Claude/隔离Codex均exit0、VERDICT: PASS，20冻结输入核同，P0/P1=0；root已具名接受三P2。两仓精确signed+DCO本地提交仍待，93仍pending/空且未开工。MTR38/91 done/complete；01原批次Mega覆盖完成，本增量Mega C/D待94、Libra仍待LTR01，均无新push。H1未交付；活动107/25，联合134/活动132。

> **历史治理修订（2026-10-20 11:39:16 UTC）：** 107/26候选R1真实Claude/Codex均FAIL；Claude的来源P2具名owner缺口已补93 AC-17/VER-17及消费条款，Codex的逐commit checkpoint保护缺口拆出MTR-101。当前108/26、活动107/25，联合134/活动132；98→101→99→11，101自身独立patch及完整C/D。R2待真实双审，未实施/验收/提交；旧105/107候选和FAIL保留。

> **历史执行登记：** 2026-10-20 10:50:49 UTC独立远端核验：MTR-38 `eca629866fb9763f2c8126ca858c43732bf6a44e` / `v0.42.57`，Docker run39405703760的arm64、amd64、docker manifest全success，main Harbor39405661929success；MTR38与91现done/complete，MTR01原批次Mega侧C/D完成，本次五文档amendment自身B/联合双审PASS、精确本地提交与94覆盖待、Libra三文档仍待LTR01。新增93～101及FIX-MTR-06/07使总数108/26、活动107/25，治理R2真实双PASS；当前前沿为两仓精确signed+DCO本地提交，93未开工/未验收，11不提前开工。H1尚未交付。

> **历史Libra整合快照（先于e4d843d整合；不作当前来源）：** 本地`3433d770`基于已获取上游`5f0e808`，两笔治理提交已重签并核父链/原文；最终fmt/clippy退出0、network nextest3/3、0skip。新治理R2已双PASS、精确本地提交c2c76b3已验签；Worktree发布冻结保持，Libra不提前push。

> **历史治理契约绑定（R2双PASS）：** `docs/refactoring/traces.md` SHA256 `771fb2807909aa0b282fa11f3c4f51b8c7bf4b9998d5d4d6c0f3dc17920cecc6`；仅DEBT-MTR-01归属改为94/100，当前38发布树的disabled HTTP/SSH仍未完成，11/39认证与92接线职责不变。历史契约SHA及PASS收据保持原值。

> **历史执行登记：** 2026-10-20 10:09:03 UTC：MTR-38已完成本地C、签名提交eca6298、main与标注v0.42.57推送及人工Release；版本Docker run39405703760尚在运行，in-progress/remote-pending，不done。MTR91继承C待D；MTR01的Mega侧待D、Libra侧仍待LTR01。Libra收据本地签名提交d1c838e，未推送；H1尚未交付。

> **历史执行登记：** 当前 2026-10-20 08:43:28 UTC：H0 R5a真实Claude/Codex双PASS；MTR-91自身双PASS、签名+DCO本地提交`3723200180b914f20af7152968438009cac373fe`已核，source-qualified；MTR-01本轮A/B已核、in-progress/locally-accepted，自身ER05及两仓精确本地提交待取得，之后才放行MTR-38。91/01均未C/D完成；Libra已原生快进141de9ab，LTR-10仍等H1/LIVE。

> **历史开工入口（2026-10-20，H0审前）：** H0来源替代双审待取得 → MTR-91当前发布基线审计 → MTR-01登记 → MTR-38 → 11→40→13→39→92→12/H1 → Libra LTR-10。97/26卡、活动96/25；91/01先A/B+双审+精确本地提交，未来38承载C/D。FIX3/LTR00取消与旧FAIL/STOP保留，旧blocked卡不批量完成。具体字段以日期计划现行执行入口为准。以下较早现场记录均为历史输入。

> **plan-20261015 历史取消快照处置（2026-10-20 07:20:41 UTC）：** FIX-MTR-03 与 LTR-00 的后续验收按用户指令取消，分别记 DEFER-TR-FIX3-AC / DEFER-LTR-06；两卡仍 blocked/空，旧 FAIL/STOP 与已发布事实保留。引用卡不自动解锁，替代依赖须另行 ER-03/G-09 两仓双审。Mega2 origin/main=74ce530d（原生 fetch 与只读 ref 一致）；Libra 原生 HTTPS fetch/快进后 main=origin/main=6003a0c（只读 ref 一致），上游 15 路径不含本轮三份计划文档。LTR-00 取消前 49 行 source pin 中三行随上游改变，现行静态脚本 exit1；不再作为历史取消快照验收。R4 双审 FAIL 已保留，R5 待审，不触发发布。

> **plan-20261015 历史决议（2026-10-20）：** 用户确认原 C2 同源材料已不存在；原件依赖的新增诊断合同停止。Mega2 本地 main `74ce530d`，本轮远端 fetch 因 SSH `LBR-NET-002` 未获核实；Libra main=origin/main `e24c2d4`，其 LTR-00 三条 source pin 已重绑但当前 B/AC-3/自身 ER-05 未闭。FIX3 AC-1/VER-5 未证、B8 FAIL、3/3 与 8/8 STOP 不变；FIX3/FIX4/MTR-01/LTR00 blocked/空，LTR01 未开工。实际前沿为只读归属审计和两仓 H0 新树双审；新现行缺陷须单轴独立卡，后继依赖不自动删改。R15/R16/R17 双 FAIL 均保留；R18 冻结输入治理修订 Claude/Codex 双 PASS（仅治理）；Mega2 远端未核实，H0 仍未 PASS；下方 03:44:17 为历史输入。

> **plan-20261015 历史现场（2026-10-20 03:44:17 UTC）：** Libra 原生快进到 `fc56869d4ad33efea085f43747494c3a4841a62b`（保留上游 CC-00..08 done/complete），Mega2 `74ce530d333aa9caee07ab74e051b5539aff937c`。旧 `ec77730` 冻结字节的治理 R11 真 Claude `VERDICT: PASS`（JSON SHA `cd6152ead3e32c60f5b57a99a95f367aca5bc7809ee64dd2fcb4c072323cfd8c`）与隔离 Codex `VERDICT: PASS`（final SHA `659d252b8780db7b64e9a6ebc9ea1f4d58243b5ab6ab577e5290761c832e8ea4`）仅覆盖旧输入。用户确认没有交接包；原 C2 原件/MANIFEST 不可核，历史数据库机制和第四调用点身份仍 unknown。旧 B8 FAIL、3/3 诊断、8/8 A/B STOP 不变；无新增诊断/A-B/发布窗口。FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-01 未开工；R13 Claude/Codex 双 `VERDICT: FAIL`：Mega2 H0 映射旧状态 P1 已修，Libra 交叉写者标题旧 HEAD P2 已修；R14 双审待取得。

> **plan-20261015 历史现场（2026-10-20 03:39:25 UTC）：** Libra 原生快进到 `fc56869d4ad33efea085f43747494c3a4841a62b`（保留上游 CC-00..08 done/complete），Mega2 `74ce530d333aa9caee07ab74e051b5539aff937c`。旧 `ec77730` 冻结字节的治理 R11 真 Claude `VERDICT: PASS`（JSON SHA `cd6152ead3e32c60f5b57a99a95f367aca5bc7809ee64dd2fcb4c072323cfd8c`）与隔离 Codex `VERDICT: PASS`（final SHA `659d252b8780db7b64e9a6ebc9ea1f4d58243b5ab6ab577e5290761c832e8ea4`）仅覆盖旧输入。用户确认没有交接包；原 C2 原件/MANIFEST 不可核，历史数据库机制和第四调用点身份仍 unknown。旧 B8 FAIL、3/3 诊断、8/8 A/B STOP 不变；无新增诊断/A-B/发布窗口。FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-01 未开工；R12 Claude `VERDICT: FAIL`（Mega2 两索引仍把旧 00:38 标当前的 P1）、隔离 Codex `VERDICT: PASS`；P1 与具名 P2 已前滚，R13 待双审。

> **plan-20261015 历史现场（2026-10-20 03:31:42 UTC）：** Libra 原生快进到 `fc56869d4ad33efea085f43747494c3a4841a62b`（保留上游 CC-00..08 done/complete），Mega2 `74ce530d333aa9caee07ab74e051b5539aff937c`。旧 `ec77730` 冻结字节的治理 R11 真 Claude `VERDICT: PASS`（JSON SHA `cd6152ead3e32c60f5b57a99a95f367aca5bc7809ee64dd2fcb4c072323cfd8c`）与隔离 Codex `VERDICT: PASS`（final SHA `659d252b8780db7b64e9a6ebc9ea1f4d58243b5ab6ab577e5290761c832e8ea4`）仅覆盖旧输入。用户确认没有交接包；原 C2 原件/MANIFEST 不可核，历史数据库机制和第四调用点身份仍 unknown。旧 B8 FAIL、3/3 诊断、8/8 A/B STOP 不变；无新增诊断/A-B/发布窗口。FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-01 未开工；现行 H0 重新双审待取得。

> **plan-20261015 历史现场（2026-10-20 00:38:10 UTC）：** Libra `main=ec77730a`、Mega2 `main=74ce530d`；R10 Claude FAIL/Codex PASS：两计划正文 DEP-TR-H0 行仍以旧 R6 为当前的 P1 已前滚，R11 终态由本字节的真实隔离双审输出裁定，不预填 PASS。FIX3 原 B8 FAIL/3+8 STOP 与 C2 原件缺失不变；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-00 AC-3/自身 ER-05 未过、formal 0/26，LTR-01 未开工；零新增诊断/A-B 窗或发布。

> **plan-20261015 历史增量记录（截至 2026-10-20 00:34 UTC）：** Libra 已原生快进到 `ec77730a43efab02ed47462f31d1d20226796b9e`，Mega2 仍为 `74ce530d333aa9caee07ab74e051b5539aff937c`；上游 6 提交及 worktree 索引增量已与本地 H0 文档合并。新 HEAD 的静态预检为 121 卡/123 边、444 相对链接零缺失；旧 49 路径 source pin 在首次检查有 8 条失效（exit1）；加入当前摘要后共重绑 9 条，同一静态脚本复核 exit0，49 行有效（输出 SHA256 `9ab2beb0e6e6bd94c2f566d8dfca19afb3703b9cc2d299c196ad0d2b80dba970`）。FIX3 原 B8 FAIL、3/3 诊断、8/8 A/B STOP 及原 C2 包缺失不变；LTR-00 自身 ER-05 双 FAIL、AC-3 未勾和交叉写者未让出，formal 0/26，LTR-01 不开工。此为同步/预检，不是新验收窗口、卡完成或发布。 旧治理 R7 冻结输入的 Claude/Codex 均明确 PASS（Claude JSON SHA `66c5fd2e85af482f4b2236b61b25a67f6e53d0ee7e44361134ec1462bcd9f1e1`、Codex final SHA `63933b2fb0f1127c44c1b69b7f909fb39305054e9bac44b00f4d806933d91e23`）；新 HEAD 整合治理 R8 首轮 Claude/Codex 均 FAIL：现行 WD 写者登记及 LTR-00 VER-1 的 441/444 当前口径两项 P1。R9 隔离 Codex PASS（final SHA `bbd9e722d31933e2f5d3ea39db522a924ba14593102afa5804a7fb0b085b9c91`）、Claude FAIL（JSON SHA `6249776b3ccf6d4e8b84bf4cc0f01feb98f4818c9ba68647b9012051c3c1cf7a`）：索引仍以旧 18:34 作当前及两计划缺 2026-10-20 修订行两项 P1；已前滚修订，R10 待最终字节双审，旧 FAIL 均保留。

## 文档职责与维护协议

本文是 mega2 不绑定具体发布日期和版本号的长期移植路线图，目标是**把 Mega 项目（`/media/eli/sky/mega`）的 Rust 后端能力完整移植到 mega2**，并在移植过程中保留 mega2 已确立的架构改进。同时，本文承载前端一致性约束：Mega 目标项目的前端与账户系统由 moon + campsite（`/media/eli/sky/campsite`）承载，mega2 的前端与账户系统是 monoui 仓库（`gitmono-dev/monoui`，sibling `../monoui`）`mega2` 分支的 `apps/next-app`；两个前端体系的功能必须保持一致（见规划原则 11 与 PT-12）。它回答"哪些 Mega 能力尚未移植、为什么、依赖什么、何时具备进入日期计划的条件"，不是 release 承诺、owner 清单或逐项实施任务表。自 PT-13 起也可登记 **mega2 原生**长期能力（事实基线是本仓 checkout，不要求 Mega revision）。具体设计、迁移、拆分、发布和回滚只进入按日期计划（`plan-YYYYMMDD.md`）或后续 RFC/ADR。

> **术语（2026-08-21）**：本文其余处出现的 *website* 是**角色名**（mega2 的
> 前端 / 认证与产品邮件投递面），不再是仓库名——其实现仓库自 2026-08-21 起为
> `gitmono-dev/monoui` 的 `mega2` 分支。派生标识（契约名 `website-mail`、
> Compose 服务 `website-next` / `website-db-init`、隔离账户库 `website`、配置键
> `MEGA_OAUTH__WEBSITE_*` / `MEGA_NOTIFICATION__WEBSITE_MAIL_*`、测试门
> `WEBSITE_IT`）**一律不改名**。

本文的持续事实来源有两类：

- **Mega 目标项目源码**（移植来源与契约基线，不是竞品或外部参照）：以 pinned revision 的 checkout 为准，逐 crate 审计移植状态；浮动 `main` 不作为规范。
- **mega2 当前 checkout**：已完成移植的事实以当前代码、测试和 `docs/refactoring/` 重构文档为准，历史计划或对 Mega 目标项目的历史描述只作为线索。

状态定义如下：

| 状态 | 含义 |
|---|---|
| 候选 | 有移植线索，但 mega2 缺口、架构适配或证据尚不足 |
| 已验证 | 已同时核对 Mega 目标项目源码与 mega2 当前源码/测试，确认缺口真实存在 |
| 已排期 | 已有按日期计划覆盖该 PT 的明确范围，并从本文链接 |
| 实施中 | 该 PT 已有已合入切片，长期完成判据仍未全部满足 |
| 已实现 | 当前可发布版本中的代码、测试、用户/兼容文档共同证明完成判据已满足 |
| 已替代 | 原移植需求仍有效，但由另一 PT 或更合适的机制承接 |
| 不采纳 | 经审计确认不适合 mega2（或明确属于范围外资产），保留编号与理由 |

只有当前 checkout 的代码、测试、兼容性与用户文档，以及可发布版本证据共同成立时，PT 才能标记"已实现"。日期计划写完、Mega 目标项目已有该能力、存在 schema 或文档声明都不构成实现证明。PT 编号一经引用不重编号；详细章节不承担排序，唯一排序入口是"长期功能总览"。

## 本次 Mega 源码审计快照

审计时间：**2026-08-11**。审计方式：分别核对两个仓库的当前 checkout 与工作区状态。（**2026-08-27 订正**：本行原写「使用 libra……两仓库均由 libra 管理，不得用 git 命令误判」；mega2 已于当日删除 `.libra` 改由 Git 管理，本仓一律用 `git` 命令核对。Mega 目标项目侧的 VCS 归属未随之变化，下次审计时须逐仓确认后再选命令，不得沿用「两仓库同构」的旧假设。上述 2026-08-11 的审计结论本身不受影响，不回改。）

- Mega 目标项目：`main` @ `3d22823e8533dd2bb7a275a92e18d8f0160f9929`（2026-08-11，`fix(identity): treat CLA as signed across username/github/public-id aliases (#2169)`）；其前一提交为 `#2168`，用于无 SQL 修复过渡期 CL reviewer。
- mega2：`main` @ `38feb4dd78edc998570df17ae9906af56a801d9e`，版本 v0.2.15。最近一次完整 Mega 同步分析仍基于 `#2129`；截至本次审计，**`#2130..#2169` 尚未逐提交归类，漂移窗口扩大。**
- mega2 工作区：除本文件外还存在用户或并行工作改动，覆盖 Git protocol、配置、测试与相关文档；本次只更新 `plan-long.md`，不将这些改动作为本次路线图结论或修改其文件。
- 关联前端与账户系统（PT-12 事实基线，2026-08-21 核对）：campsite（`/media/eli/sky/campsite`，Rails 应用，Mega 前端 moon 的账户/后端配对，Mega 侧引用不在本次改指范围）；**前端仓库自 2026-08-21 由 `genedna/website` 改指 `gitmono-dev/monoui`**（sibling `../monoui`，libra 管理），当前 checkout 为 `mega2` 分支且工作树 clean。执行 PT-12 对照或联动发布前，仍须记录该分支的 pinned revision（唯一事实源：[`../refactoring/website-auth.md`](../refactoring/website-auth.md) §头部）；其 `apps/next-app`（Next.js）仍是 mega2 的前端与账户系统。

Mega workspace crate 审计表（15 个 Rust crate + 前端与非 Rust 资产）：

| Mega 资产 | 职责 | 移植状态 | mega2 对应 | 证据入口 |
|---|---|---|---|---|
| `mono` | 主二进制：CLI、service(http/ssh/multi)、HTTP API 全套路由、bootstrap、server、git_protocol、email、notification、orion_build_dispatch | 已移植（高度重构；本仓 SMTP/`src/mail` 已按 ADR-WA-08 退场，产品邮件投递归属 website） | `src/cli.rs` + `bin/`、`src/commands/`、`src/server/`、`src/api/`、`src/context/`、`src/contract/git_protocol/`、`src/notification/`、`src/bellatrix/` | 各 crate `src/` 顶层结构对照；邮件契约见 `docs/refactoring/website-mail.md` |
| `ceres` | monorepo 领域库：application、bus、diff、infra、lfs、merge_checker、model、transport | 部分移植 | `src/ceres/`（api_service/build_trigger/code_edit/diff/lfs/merge_checker/model/pack/protocol）；application/artifact→`src/jupiter/service/artifact_service.rs`、application/buck→`buck_service.rs`、application/webhook→`webhook_service.rs`、application/notification→`src/notification/` | **`ceres/bus`、`ceres/infra` 未作为模块移植（PT-05）；Mega #2138/#2139 已把二者纳入 application/transport 分层，必须按新路径审计，不得按旧目录直接复制** |
| `jupiter` | 存储层：model、redis、service、storage、tests、utils | 已移植 | `src/jupiter/` | 同名目录结构 |
| `jupiter/callisto` | sea-orm 实体 | 已移植 | `src/callisto/` | 同名目录结构 |
| `jupiter-migrate` | sea-orm 迁移 | 已移植 | `src/jupiter/migration/` | 同名目录结构 |
| `common` | config、enums、errors、utils | 已移植（config 大幅扩展） | `src/common/`；`common/config` 提升为一级 `src/config/` | `docs/refactoring/config.md` |
| `saturn` | Cedar 策略/授权 | 已移植 | `src/contract/policy/`（并吸入 mono 的 `api/guard/`） | `docs/refactoring/contract.md` |
| `vault` | RustyVault 集成 | 已移植 + 已加固：形态为 crates.io `libvault` 0.3.0 + 集成层（vendored `src/vault/` 于 2026-08-21 删除，见 [`plan-20260820.md`](plan-20260820.md)） | `libvault` crate + `src/contract/vault/` | `docs/refactoring/vault.md`；ADR-VLT-01 |
| `api-model` | API DTO | 已移植 | `src/contract/api/` | `docs/refactoring/contract.md` |
| `io-orbit` | 对象存储 | 已内联 | `src/orbit_api/` + `src/orbit/`，经 `build_object_storage` 直连工厂 | `docs/refactoring/orbit.md` |
| `clients/orion-client` | orion-server HTTP 客户端 | 部分移植 | `src/bellatrix/`（仅 build dispatch 路径） | 完整 API 面未核对（PT-08） |
| `clients/orion-scheduler-client` | orion-scheduler HTTP 客户端 | **未移植** | 无 | PT-08；Mega #2143/#2146/#2150/#2159/#2160 已扩展 runner provisioning、multi-VM、磁盘状态和 VM 元数据面 |
| `orion` | 构建执行 agent（antares、buck_controller、disk、repo、ws、api） | **未移植** | 无 | PT-07 |
| `orion-server` | 构建任务服务端（api、buck2、log、model、repository、scheduler、service、server） | **未移植** | 无（仅保留消费侧 `buck_router`/`artifacts_router`/`build_trigger_router` API 面） | PT-06；Mega #2163 新增无 idle worker 时最长 60 分钟的构建排队语义 |
| `orion-scheduler` | QEMU VM 弹性调度（vm_manager、vm_cleanup、keep_alive、orion_deployer、webhook） | **未移植** | 无 | PT-08 |
| `moon` | Web 前端（pnpm + turbo，Next.js） | 范围外 | 无（引擎只移植 Rust 后端） | 见"不进入本长期移植计划的 Mega 资产" |
| `tests/` | Git 协议/对象层集成测试夹具（data、diff、objects、refs、scripts） | **未移植** | mega2 有 `test/project/` 与 `bin/tests/`，协议级夹具未系统移植 | PT-04 |
| `scripts/`、`docker/` | 运维/部署辅助（crates-sync、init_mega、demo、deployment） | 未移植 | 无 | 是否移植需单独决策，见"不进入"章节 |

最近审计记录最多保留 12 次；超过上限后把更早记录压缩为 revision 集合与结论，不保留逐 crate 对照日志。

| 审计日期 | Mega revision | mega2 基线 | 路线图结论 |
|---|---|---|---|
| 2026-08-26 | `2398a92`（含 #2175） | v0.3.5（`91df97c`） | [`plan-20260826.md`](plan-20260826.md) 收口窗口 #2130→#2175：LFS lock list limit 校验与 400 分类（#2175）、import attach `.gitkeep` blob 持久化（#2152）、CL 列表 build_status worst-wins 聚合与回填（#2163 附带项）已交付（SYNC-01..05）；campsite 身份模型（#2165–#2170）、ceres application/transport/bus 结构重构（#2138/#2139/#2142）、locks/verify 400 分类登记为 DEFER-SYNC-02/03/05，不改变 PT 状态。 |
| 2026-08-11 | `3d22823`（含 #2169） | v0.2.15（`38feb4d`） | 复核确认 PT-01 已完成、PT-04 已进入日期计划；PT-03 已吸收 auth、delete-only、per-channel state 与 capability 收敛，只余 streaming 和完整矩阵。对象存储/Redis SecretRef、Slack/Webhook、Vault file audit sink 已交付，分别从 PT-10/PT-09/PT-11 的缺口移除。Mega #2130..#2169 仍未完成逐提交归类；新增账户审批/Cedar 管理、Orion queue/runner/VM 表面扩大 PT-02/PT-06/PT-08/PT-12 的审计范围；#2165..#2169 的 identity/Cedar reviewer 域（Cedar reviewer 解析、campsite_user_id、admin 检查、CLA 签名）进一步扩大 PT-02/PT-12 的对照范围。 |
| 2026-08-03 | `42cd288d`（含 #2163） | v0.2.1（`39f74332`） | 复核确认 PT-01 已完成、PT-04 已进入日期计划；PT-03 已吸收 auth、delete-only、per-channel state 与 capability 收敛，只余 streaming 和完整矩阵。对象存储/Redis SecretRef、Slack/Webhook、Vault file audit sink 已交付，分别从 PT-10/PT-09/PT-11 的缺口移除。Mega #2130..#2163 仍未完成逐提交归类；新增账户审批/Cedar 管理、Orion queue/runner/VM 表面扩大 PT-02/PT-06/PT-08/PT-12 的审计范围。 |
| 2026-07-27 | `d2b6d1c3`（#2157） | v0.1.50（`562122c2`，同步分析基于 #2129） | 首版：确认 orion 三件套、ceres/bus+infra、协议测试夹具为主要缺口；已移植模块存在 #2129→#2157 漂移窗口；集成测试基建（PT-01）排为下一个执行任务，Mega 基线追平（PT-02）紧随其后 |

**本次结论：PT-01（集成测试基建扩展与统一）已完成；PT-04 已由 `plan-20260803.md` 承接。orion 三件套仍是最大整体缺口，且上游的 queue、runner provisioning、multi-VM 与磁盘压力语义已扩大其移植基线。新增的 #2165..#2169 属 identity/Cedar reviewer 域，不改变任何 PT 状态或优先级，但扩大 PT-02（逐提交归类）与 PT-12（双前端账户/认证语义对照）的审计范围。**

## 规划原则

以下原则适用于全部 PT。原则 1–4、8 只约束 Mega 移植项（PT-01..PT-12）；原则 5–7、9–12 对 mega2 原生项同样生效。

1. **忠实移植优先于重新设计。** 默认保留 Mega 的 wire 行为、DB schema、API 契约和错误语义；架构性偏离必须是已决 ADR 并记录在案（如 contract 归并、config 提升、workspace 拆分），不得在执行中临时发明。
2. **mega2 的架构改进不回退。** 单 package `mega2`（lib `mega2_core` + `[[bin]]`）、`src/contract/` 边界归并、一级 `src/config/`、`src/notification/`（邮件投递已迁 website，见 ADR-WA-08）、Vault（crates.io `libvault` + `src/contract/vault/` 集成层加固；不得回流为内嵌 vendored fork，亦不得因与 Mega `libvault-core` 目录差异而回退——依赖形态演进见 [`plan-20260820.md`](plan-20260820.md) ADR-VLT-01）、orbit 内联于 `src/orbit_api/` + `src/orbit/`（见 [`plan-20260824.md`](plan-20260824.md)）、对象存储构造时机等已交付改进，不因"与 Mega 不一致"而改回。
3. **移植前必须 pin 并核对 Mega revision。** 每个日期计划开工前用 libra 记录 Mega 实际 checkout revision，逐文件刷新源码锚点；不得把浮动 `main`、历史同步报告或本文快照当作当前事实。
4. **先追平、后扩展。** 已移植模块的 Mega 基线漂移（PT-02）优先于在其上叠加新能力；在漂移窗口上实施新 PT 前，必须先确认相关模块的 Mega 变更已被吸收或明确排除。
5. **三门验收是硬门禁。** 每个任务至少通过 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test ...` 指定用例；`cargo build [--tests]` 保持 0 错误 0 警告。
6. **机器接口与文档同步。** 涉及公开命令、配置项、DB schema、HTTP API、错误码或存储格式的移植，必须同步 OpenAPI schema、配置示例、错误码文档和测试，并遵守 `AGENTS.md` 的实体/迁移/子命令登记流程。
7. **安全边界 fail-closed。** Vault、SecretRef、认证授权、协议输入解析的安全语义不得因移植而弱化；Mega 目标项目的宽松默认不自动成为 mega2 的默认。
8. **不重复建设事实源。** Mega 侧能力若已被 mega2 以更强形态覆盖（如 notification、config、mail），不回流旧实现；只补缺口，不重复移植。
9. **测试随代码移植。** 移植功能必须携带或重建其测试；禁止手写 schema SQL，必须走真实 migration；集成测试沿用 docker-compose 测试栈与 `bin/tests/` 黑盒分层。
10. **计划状态必须据代码更新。** 每次审计重新读取当前 `src/`、migration、API 路由和相关测试；不得复制上次"当前基础"文字代替复核。
11. **双前端一致性。** Mega 前端体系是 moon + campsite，mega2 前端与账户系统是 monoui `mega2` 分支的 `apps/next-app`；mega2 的公开 API 与账户行为必须与 `apps/next-app` 对齐，且两个前端体系的功能保持一致。任何新增或变更后端公开行为的 PT/日期计划，必须包含对两侧前端的影响评估；不允许单侧漂移（详见 PT-12）。storage-only 无登录面的能力（如 PT-13）写 `N/A` 与原因即可。
12. **mega2 原生项续编编号。** 非 Mega 移植的长期能力使用 `PT-13` 起的新编号，不得占用或重排 `PT-01..PT-12` / `SB-01..SB-03`。事实基线是本仓源码、测试、配置与 `docs/refactoring/`；Mega 侧无对应物时证据列写 `N/A`。

## 当前基础

以下事实已在 2026-07-27 以当前 mega2 checkout（v0.1.50，`562122c2`）的源码、测试与 `docs/refactoring/` 文档复核；历史计划不作为实现证据：

| 基础能力 | 当前事实 | 长期规划中的用途 |
|---|---|---|
| callisto 实体 + jupiter storage/migration/redis | 已从 Mega 移植并持续对齐（最后同步基于 Mega #2129） | 所有 PT 的数据层基础 |
| `src/config/` 一级配置体系 | LoadMode、SecretRef/resolver、`config` 命令族、集中校验、Profile、测试分层、受控热加载已交付；对象存储与 Redis 凭据均已支持 post-vault `SecretRef`（`docs/refactoring/config.md`、`vault.md`） | PT-10 的起点；所有服务的配置承载 |
| `src/contract/` 边界归并 | api/git_protocol/policy/vault 四域归并完成，旧路径无兼容层（`docs/refactoring/contract.md`） | 新移植模块的落点规范 |
| Vault（crates.io `libvault` 0.3.0 crate + 集成层） | A–J 阶段可交付子集完成：fail-closed、DB-only bootstrap、root token 退役、unseal share rekey、backup/restore、可配置 file audit sink 和 fail-closed 审计策略（`docs/refactoring/vault.md`）；依赖形态已由 vendored 迁回 crates.io `libvault`，UN-31 只读引导在集成层重建（[`plan-20260820.md`](plan-20260820.md)） | PT-11 的起点；凭据类 PT 的前置 |
| `src/notification/`（出站只剩 generic webhook） | 本仓出站只保留 `[notification.webhook]`（外加无关的 `[storage_events]`）；SMTP / Slack / in-app inbox / website-mail 客户端已拆除（[`plan-20260919.md`](plan-20260919.md)）；渠道凭据经 SecretRef | PT-09 的起点 |
| orbit 对象存储（内联） | `src/orbit_api/` + `src/orbit/` 单 package；`object_store` cloud features 在 core 编译图 | LFS/artifact/构建产物的存储承载 |
| Git smart HTTP/SSH 协议 | `info/refs` 严格化、fallible pkt-line parser、SSH exec parser、per-channel state、认证上下文、delete-only receive-pack、capability truth table、HTTP LFS 边界与首批真实 CLI smoke 已交付；HTTP/SSH 仍完整缓冲请求/通道（`docs/refactoring/protocol.md`） | PT-03/PT-04 的起点 |
| bellatrix（orion-client 部分移植） | build dispatch 路径可用，替代 Mega mono 的 `orion_build_dispatch.rs` | PT-06/PT-07/PT-08 的客户端侧基础 |
| 集成测试基建 | docker-compose 测试栈（Postgres/Redis/RustFS/git-cli/`website-next`，`-p mega2-it`）、`integration_vault` / `integration_git_cli` / `integration_website_auth` 黑盒；本仓数据面不启动 SMTP 捕获服务；CI `config-validation.yml` 与 `git-protocol-smoke.yml` | 全部 PT 的验收承载；PT-01 收口最小矩阵；PT-04 的 SSH/LFS 与完整协议矩阵已进入 `plan-20260803.md` |

## 长期功能总览

| ID | 能力 | 优先级 | 状态 | 当前判断 | Mega 证据（revision `3d22823`） | 已关联日期计划 | 最近验证 |
|---|---|---:|---|---|---|---|---|
| PT-01 | 集成测试基建扩展与统一 | P0 | **已完成**（D 组绿于 v0.1.177，2026-07-30） | git-cli runner、HTTP 最小矩阵、热加载/多渠道扇出黑盒、并发 dispatcher 基线与 CI 执行/触发面已落地；P2：`config init`/热加载/多渠道扇出已落地，次渠道 retry（`DEFER-IT-10`）、GCS（`DEFER-IT-08`）、全局 disabled vs `system_required`（`DEFER-IT-07`）书面关闭；SSH/LFS 完整矩阵与真多进程黑盒仍属 PT-04/PT-09 | `mega/tests/`（对照）、`docs/refactoring/integration.md` P2 清单 | [`plan-20260727.md`](plan-20260727.md)（IT-01、IT-02、IT-03、IT-04、IT-06、IT-07、IT-08、IT-09、IT-10、IT-11、IT-12；IT-05 已关闭为非任务卡） | 2026-07-30 |
| PT-02 | 已移植模块 Mega 基线追平与持续同步 | P0 | 已验证 | mega2 同步基线停在 Mega #2129，Mega 已到 #2169；callisto/jupiter/ceres/mono 对应面需逐模块核对漂移，重点包括 #2138/#2139 分层重构、#2145 账户审批、#2147 Cedar 管理、#2152 对象缺失处理，以及 #2165..#2169 的 identity/Cedar reviewer 域 | Mega log #2130–#2169 | 无 | 2026-08-11 |
| PT-03 | Git 协议兼容性与 LFS 收尾 | P0 | 部分完成 | auth 上下文、delete-only push、SSH per-channel state、capability truth table、LFS 路径/认证边界已交付；**repo/path 级 push ACL / Cedar 三态 push 门**已由 [`plan-20260812.md`](plan-20260812.md)（REL-01 UN-02/UN-03/UN-13）交付；FastCDC Media 与 receive-pack report-status/CAS 已由 [`plan-20260901.md`](plan-20260901.md) 交付；仍缺 streaming pkt-line 与完整 CLI 故障/并发矩阵 | `mega/mono/src/git_protocol/`、`mega/ceres/src/transport/` | 无 | 2026-08-03 |
| PT-04 | Git 协议集成测试夹具与真实 CLI 兼容矩阵 | P0 | 实施中 | `plan-20260803.md` 已交付 HTTP `pull`、`anonymous_access=false` 真实客户端拒绝、HTTP LFS 往返、cargo-native SSH clone/pull/push 与坏密钥拒绝矩阵并接入 CI；Mega 夹具审计结论 NO-GO（`DEFER-GM-03`，`docs/refactoring/mega-git-fixtures-audit.md`）；剩余 `DEFER-GM-01/02/04/05`（ImportRepo 矩阵、pure SSH LFS、shell SSH 广度 CI、missing-repo 探针）为 follow-up | `mega/tests/{data,diff,objects,refs,scripts}` | [plan-20260803.md](plan-20260803.md)（GM-* / GM-R1） | 2026-08-12 |
| PT-05 | ceres/bus 事件总线与 infra 基础设施归位 | P1 | 已验证 | `TransportRuntime`/`TransportEvent`/`ApplicationEventHandler` 未移植；cache 已并入 api_service，pack_decode/pack_stream 逻辑散落 | `mega/ceres/src/bus/`、`mega/ceres/src/infra/` | 无 | 2026-07-27 |
| PT-06 | Orion Server 构建控制面移植 | P1 | 已验证 | 整体缺失；mega2 仅保留消费侧 API 面（buck/artifacts/build_trigger router）与 bellatrix 客户端 | `mega/orion-server/`、`mega/mono/src/api` 的 orion_runner_router | 无 | 2026-07-27 |
| PT-07 | Orion 构建执行 Agent 移植 | P1 | 已验证 | runner（ws 客户端、buck_controller、disk/repo 管理）整体缺失 | `mega/orion/` | 无 | 2026-07-27 |
| PT-08 | Orion Scheduler、VM 弹性调度与客户端 API 面补齐 | P2 | 已验证 | scheduler 与 orion-scheduler-client 未移植；bellatrix 仅覆盖 build dispatch，完整 OrionBuildClient API 面未核对 | `mega/orion-scheduler/`、`mega/clients/` | 无 | 2026-07-27 |
| PT-09 | 通知与邮件能力对齐收尾 | P2 | 实施中 | **本仓出站只剩 generic webhook**（`[notification.webhook]`；外加无关的 `[storage_events]`）。SMTP / Slack / in-app inbox / website-mail 客户端已拆除（[`plan-20260919.md`](plan-20260919.md)）。剩余：build 完成触发器、次渠道 retry（`DEFER-IT-10`）、真实多进程/多实例黑盒矩阵（对象改为 webhook，不在 60919 实现） | `mega/mono/src/notification/` | [`plan-20260731.md`](plan-20260731.md)（MN-01..MN-06）；[`plan-20260802.md`](plan-20260802.md)（WE-* / DEP-06）；[`plan-20260919.md`](plan-20260919.md)（出站拆除） | 2026-09-19 |
| PT-10 | 配置体系与 SecretRef 收尾 | P2 | 实施中 | 对象存储与 Redis SecretRef、`[oauth]`、跨 source/profile diagnostics 和首批热加载黑盒均已落地；剩余为真实消费者订阅清单、跨 await 生命周期审计与持续扩展，而非启动顺序改造 | `mega/common/src/config`（基线对照） | [`plan-20260731.md`](plan-20260731.md)（AU-02；`[oauth]`） | 2026-08-03 |
| PT-11 | Vault 安全工程收尾 | P2 | 实施中 | file 持久化 audit sink、可选 fail-closed、backup/restore、unseal share rekey 已交付；仍缺 KEK 轮换（无 RustyVault 原语）、异地/HTTP audit sink、外部托管 root recovery 与格式版本策略；**依赖形态**（vendored → crates.io `libvault` 0.3.0 + UN-31 只读模式集成层重建）已由 [`plan-20260820.md`](plan-20260820.md) 于 2026-08-21 交付，本 PT 安全收尾缺口不变 | `mega/vault/`（基线对照） | [`plan-20260820.md`](plan-20260820.md)（依赖形态；非 KEK/审计 sink） | 2026-08-21 |
| PT-12 | 前端与账户系统一致性（website `apps/next-app` ↔ Mega moon+campsite） | P1 | 候选 | **会话信任路径与 compose 同栈 IT**（website-next + `integration_website_auth`）已实现；**身份键迁移**已由 [`plan-20260812.md`](plan-20260812.md) UN-05 handoff 移交本 PT（DEP-04 outgoing，实际移交 **2026-08-17**；DEFER-UN-04 八项承接约束）；Mega #2145 账户审批、#2147 Cedar 管理及 #2165..#2169 的 identity/Cedar reviewer 域扩大全量 moon↔`apps/next-app` 对照范围；monoui `mega2` 分支 pin 须执行期确认；全量对照仍候选 | monoui `apps/next-app`、`mega/moon/`、campsite | [`plan-20260731.md`](plan-20260731.md)（AU/ITW）；handoff [`plan-20260812.md`](plan-20260812.md) UN-05/DEP-04 | 2026-08-17 |
| PT-13 | storage-only 统一推送密文（同一 `[[git.push_tokens]]` 用于 HTTP 与 SSH receive-pack） | P1 | 候选 | 查找与身份已统一：HTTP 写 / SSH 读共用 `lookup_push_token`。SSH **写**仍被 TP-20 关掉（`DEFER-SP-02`）。本方案在 `push_auth=token` 下显式打开 SSH receive-pack，不新开第二套凭据，也不把 `none` 的匿名写扩到 SSH | N/A（mega2 原生） | 前置事实 [`plan-20260905.md`](plan-20260905.md) TP-20、[`plan-20260908.md`](plan-20260908.md) SP-* / `DEFER-SP-02`；尚无承接日期计划 | 2026-09-16 |
| PT-14 | trunk 历史只读投影视图 | P0 | 实施中 | P0 已由 [`plan-20261002.md`](plan-20261002.md) 交付；长期剩余：DEFER-HP-01、DEFER-HP-02、DEFER-HP-03、DEFER-HP-04、DEFER-HP-05、DEFER-HP-06、DEFER-HP-07、DEFER-HP-08、DEFER-HP-09、DEFER-HP-10、DEFER-HP-11、DEFER-HP-12、DEFER-HP-13、DEFER-HP-14、DEFER-HP-19。Docker 远端结果独立由 DEFER-HP-20 承接 | N/A（mega2 原生） | [`plan-20261002.md`](plan-20261002.md)（P0）；P1 尚无承接日期计划 | 2026-10-08 |
| PT-15 | `v0.42.25` Docker 镜像按原提交精确补发 | P1 | 已验证（待排期） | 历史 Docker run 失败，版本镜像缺失；只补不可变的 `v0.42.25` / `0.42.25` 标签，不改 `latest`、不把旧 run 写成成功。接收 [`plan-20260920.md`](plan-20260920.md) 的 `DEFER-OX-02`；与 PT-14 的全部历史 D 结果债务 `DEFER-HP-20` 分开核对 | N/A（mega2 原生发布债务） | [`plan-20260920.md`](plan-20260920.md)（outgoing `DEFER-OX-02`；本计划仅验证新 tag 的 D 门）；补发执行尚无日期计划 | 2026-10-09 |
| PT-16 | 外部 AI operator 实际接收与执行 | P1 | 候选（待指定目标） | 本仓只交付 Q7 status 拉取 / 具名命令接口及 fake-client 回环；真实 AI 目标的身份、接收、决策、执行与审计仍无证据。接收 [`plan-20260920.md`](plan-20260920.md) 的 `DEFER-OX-03` | N/A（mega2 原生集成） | [`plan-20260920.md`](plan-20260920.md)（outgoing `DEFER-OX-03`；OX-56/53 本地交接）；外部集成尚无日期计划 | 2026-10-09 |
| PT-17 | Scoped Traces归档与Agent历史检索 | P0 | MTR100源码、A/B、自身与最终双审、0.42.60完整C、签名提交、main/tag/人工Release及适用D已实际闭合：done/complete。Mega6/109（活动6/108）、Libra0/26（活动0/25），联合6/135（活动6/133）。DEBT-MTR-01仅disabled HTTP94+SSH100拒绝边界closed；enabled/H1/LTR01仍待。 **历史索引：** FIX08已done/complete，Mega完成5/109（活动5/108）、Libra0/26（活动0/25）。MTR100六非Cargo路径已恢复，现in-progress；locally-accepted仅保留历史A/B输入资格，新组合树九门A及完整18项smoke未执行，新自身Review与0.42.60完整C/D待。 **历史索引：** MTR94 v0.42.58完整C/D已核、done/complete；93继承精确94覆盖而done/complete（来源限94/100）；01仅Mega覆盖完成、Libra仍待LTR01。Docker双架构/manifest及Harbor实际success。Libra本地signed b157396、未push，H1/LTR01仍待；100已核ER01/02/03并开始实现，验收/双审/C/D待。 **历史D待定索引：** MTR94源码/A-B、自身R2及C4后置最终真实Claude/Codex双PASS已核；C4五门exit0、编译0警告/错误，full2979通过/0失败/3原有ignored（20目标）。精确13路径signed+DCO提交`04c42e10ba88ed45b830d509407d5732f4f17845`已推main；标注`v0.42.58`及人工Release已核。版本Docker run39539957757与main Harbor run39539882313均已触发，最后观察运行中，D尚待；94为in-progress/remote-pending，93覆盖及100前驱未闭合，100未开工。 Libra选定上游`3b1200a80e00e8d1a8866ab07aa790c27f8a17a9`、0.30.69资格保留：full10658与network3实际PASS，21原有ignored、1 flaky及future-incompat警告不隐去；三份计划文档精确signed+DCO提交`9314a17348aceb0c631139715fd213ead73ed90c`已验签，仅本地未push。H1/LIVE/WRITERS与Worktree发布冻结、LTR01唯一发布点均不变，formal0/26。 **历史索引状态：** MTR94源码与A/B已通过（HTTP3/3、模块8/8、API433/0/1既有ignored、release Git smoke17/17、fmt/clippy与release build退出0），in-progress/locally-accepted；自身ER05及完整C/D待，未提交/推送。93受限source-qualified仅94/100且C/D94待；Libra已整合e4d843d至e2509aa，新树full/network待，H1未交付。 **历史12:26记账：** MTR93的17AC/17VER、文档B、自身真实Claude/Codex双PASS和精确signed+DCO交接已核；Mega `2320e95a80b5d813534d9ce8696baff1b7aab51f`、Libra `79f7cfaddbc72070a9b8d2a42c06410387c07fef`均仅本地未push。93为source-qualified（仅94/100拒绝路径）、in-progress/locally-accepted，C/D94待。MTR94于12:22:51 UTC完成ER01/02/03正式开工，in-progress/空，源码实现中；6AC/1VER全部未勾，验收/自身双审/版本及C/D均未执行。38/91 done/complete，01/93本增量Mega覆盖待94，Libra待LTR01；H1未交付，旧53风险/28卡blocked/取消及FAIL/STOP保持。总108/26、活动107/25。 **历史索引输入：** 历史取消快照 2026-10-20 07:20:41 UTC：Libra main=origin/main=6003a0c，取消前 source pin 三行漂移、旧静态脚本 exit1；FIX3/LTR00 验收取消，引用方继续阻塞，替代依赖待两仓双审。历史 2026-10-20 06:36:12 UTC：FIX3/LTR00 验收取消，DEFER-TR-FIX3-AC/DEFER-LTR-06；两卡 blocked/空、引用方不自动解锁，替代依赖待两仓双审。历史 2026-10-20：Libra `e24c2d4`、Mega2 本地 `74ce530d`，Mega2 远端 SSH fetch 未核实；原 C2 材料不存在，停止原件诊断；R15/R16/R17 双 FAIL 均保留；R18 冻结输入治理修订 Claude/Codex 双 PASS（仅治理）；Mega2 远端未核实，H0 仍未 PASS；FIX3/FIX4/MTR-01/LTR00 blocked/空，LTR01 未开工。历史 2026-10-20 03:44:17 UTC：Libra `fc56869`、Mega2 `74ce530d`；R13 Claude/Codex 双 FAIL 的 H0 映射旧状态 P1 已前滚，R14 双审待取得；无包 STOP、旧 B8/3+8 与 FIX3/LTR00 blocked 不变。历史 2026-10-20 03:39:25 UTC：Libra `fc56869`、Mega2 `74ce530d`；R12 Claude FAIL/Codex PASS 的旧快照 P1 已修，R13 双审待取得；无包 STOP、旧 B8/3+8 与 FIX3/LTR00 blocked 不变。历史 2026-10-20 03:31:42 UTC：Libra `fc56869`、Mega2 `74ce530d`；R11 旧 `ec77730` 输入双 PASS；用户确认无交接包，FIX3 同源诊断 STOP，旧 B8/3+8 保留；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-01 未开工；历史取消快照 H0 新审待取得。历史 2026-10-20 00:38:10 UTC：Libra `main=ec77730a`、Mega2 `main=74ce530d`；R10 Claude FAIL/Codex PASS：两计划正文 DEP-TR-H0 行仍以旧 R6 为历史取消快照的 P1 已前滚，R11 终态由本字节的真实隔离双审输出裁定，不预填 PASS。FIX3 原 B8 FAIL/3+8 STOP 与 C2 原件缺失不变；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-00 AC-3/自身 ER-05 未过、formal 0/26，LTR-01 未开工；零新增诊断/A-B 窗或发布。 历史 2026-10-20 00:34 UTC：新 HEAD `ec77730` 的 49 路径/444 链接静态门通过；R9 Claude FAIL/Codex PASS 的索引与修订历史 P1 已前滚，R10 待双审。FIX3、FIX4、MTR-01 与 LTR-00 仍 blocked/空，零新诊断/A-B 窗；LTR-01 未开工。 历史 2026-10-19 18:34 UTC：新 FIX3 条件治理终态 R6 Claude/Codex 双 VERDICT: PASS（治理范围）；Claude 两项 P2 已具名修正：Libra R5 轮次加“新提案”限定、LTR00 B 静态摘要补 AC-4。旧失败/STOP、零新窗口及所有卡阻塞不变；本次纯文档修订的结论以隔离终态审查输出为准。 历史 2026-10-19 18:30 UTC：新 FIX3 条件恢复治理 R5 Claude/Codex 双 VERDICT: PASS（冻结输入，仅治理）；收据与 H0/LTR00 B 静态限定已前滚记账，终态文档待独立复核。原 C2 包、精确诊断合同、新窗口及交叉写者让出仍缺；FIX3/MTR-01/FIX4/LTR00 blocked/空，无卡验收、提交或发布授权。 历史 2026-10-19 18:25 UTC：新 FIX3 提案 R4 Claude/Codex 双 FAIL；LTR-00 现行 AC-1/AC-2/VER-2 曾写“待 R3”与已发生 R3 冲突，三处已改为不依赖轮次的“现行 H0 待双 PASS”；R5 待审。原 C2 包与精确诊断合同仍缺，零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空。 历史 2026-10-19 18:21 UTC：新 FIX3 提案 R3 Claude FAIL/Codex PASS；Claude P1 为 Libra 生效计划总览仍以旧 18:08 作为历史取消快照，已修；审查 R4 待双 PASS。恢复方案仍须原 C2 包与另审精确诊断合同；零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空。 历史 2026-10-19 18:16 UTC：新 FIX3 提案 R2 Claude/Codex 均 FAIL；原全量约62分钟及连接压力与拟定60分钟诊断上限冲突、Mega2 活跃索引仍称旧 R6 历史取消快照，两项 P1 已前滚修订，R3 待审。旧 B8 FAIL、3/3、8/8 STOP 保留；零新诊断/A-B 授权，FIX3/MTR-01/FIX4 与 LTR-00 blocked/空。 历史 2026-10-19 18:08 UTC：FIX3 提案联合首轮 Claude PASS/Codex FAIL，两项生效索引 P1 已修、第二轮待审；零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空、62张 pending。历史 2026-10-19 18:02 UTC：FIX3 提案待双审，LTR-00 自身双 FAIL/交叉写者 P1 未闭。历史 2026-10-19 17:39:15 UTC：H0 R6 Claude/Codex 双 PASS，当时仅 LTR-00 治理审计可继续。历史 2026-10-19 17:33:08 UTC：H0 R5 Claude FAIL/Codex PASS，日期索引P1已修、R6待审；历史 2026-10-19 17:28:17 UTC：H0 R4 Claude FAIL/Codex PASS，主表索引冲突已修、R5待审；FIX3/MTR-01/FIX4 blocked/空，其他27张已发布依赖后继及MTR-10 blocked/complete，62张 pending。历史：2026-10-17 22:58:26 UTC MTR-77 done/complete，0.42.37，`707ddd8`，Docker run `39076153505` success，下一张卡 MTR-78 | 新增文档B/双审/精确提交待；FIX06须开工资格核验。 **历史入口：** MTR100恢复后首次组合树资格待：九门A拟一次7200秒、原逐门期限不变；smoke拟单独7200秒总窗口、保留原单门期限并绑定新release二进制；旧工具链binary原件不足，不carry旧A等价资格。 **历史入口：** FIX08已完成源码、A/B、真实ownER05与最终交付审查、本地五门C、精确签名+DCO提交、main/tag/人工Release及适用D：done/complete。Mega正式完成5/109（活动5/108）、Libra仍0/26（活动0/25）。MTR100仅其FIX08发布前驱解除，可按受保护前像精确恢复六个非Cargo路径；恢复尚未在本收据执行，MTR100自身输入/Review/0.42.60完整C/D仍待，不能据此标本卡完成。 **此前阶段索引：** FIX08源码、六门A/B及ownER05双PASS已核；0.42.59最终五门本地C实际通过（session68149 exit0，20目标2981通过/0失败/3原有ignored/0过滤/0measured，fmt/strict clippy及双build零warning/error）。Lifecycle=in-progress、Acceptance=locally-accepted；尚未提交/推送/tag/人工Release/D，不能done/complete。MTR100 WIP仍受保护；Libra仍待新上游资格/H1/LTR01。 **此前阶段索引：** FIX08 in-progress/locally-accepted；2AC/3VER及六门A/B已核，自身ER05真实Claude/Codex双PASS（session82523 exit0）；版本入口实际session25405 exit0，58→59两面一致。完整C尚未启动；签名提交、推送/tag/Release/D均待。MTR100 WIP继续受保护，未恢复；Mega完成仍4/109、Libra0/26。 **历史阶段索引：** FIX08六门A/B原件已核（2026-10-21T10:23:18.538031+00:00）：in-progress/locally-accepted，2AC/3VER已核；ownER05、版本bump、完整C、签名提交、推送/tag/Release/D均未执行。MTR100 blocked/locally-accepted，受保护WIP尚未恢复；完成计数仍Mega4/109（活动4/108）、Libra0/26（活动0/25）。 **此前索引快照：** FIX08采纳候选待双审；Mega109/活动108、Libra26/活动25、联合135/活动133；完成仍4/109与0/26。D1入口STOP、未运行测试且claim已消费；MTR100 blocked/locally-accepted，独立FIX08完整C/D前不放行。 **此前索引快照（保留历史）：** MTR94 v0.42.58完整C/D已核、done/complete；93继承精确94覆盖而done/complete（来源限94/100）；01仅Mega覆盖完成、Libra仍待LTR01。Docker双架构/manifest及Harbor实际success。Libra本地signed b157396、未push，H1/LTR01仍待；100已核ER01/02/03并开始实现，验收/双审/C/D待。 **历史D待定索引：** MTR94五门与最终双审PASS，signed04c42e1/main/标注v0.42.58/人工Release已核；实际Docker39539957757及Harbor39539882313 D待，remote-pending，93覆盖与100前驱未闭合；Libra资格10658/network3及signed9314a17已核、未push，H1/LTR01仍待。本次后置事实文档另做B/双审。 **历史提交前索引：** 当前C4本地五门通过，2979/0/3、20目标，0.42.58；最终输入审查/签名提交/发布C-D待，93覆盖与100前驱仍未解除；Libra10658/network3与本地signed2332823已核，H1/LTR01仍待。 以下为历史阶段快照： MTR-38 A/B已通过（resolver8/8、v0 3/3、v2 2/2、traces模块9/9、真实Git smoke17/17、nightly fmt与strict clippy exit0），当前in-progress/locally-accepted；自身Claude/Codex ER05待，尚未bump/commit/push/tag/Release/C-D。MTR91/01仅source-qualified；Libra LTR10仍等H1/LIVE，取消验收与旧FAIL/STOP保留。 历史登记： 当前 2026-10-20 08:57:23 UTC：MTR-91与MTR-01均已A/B、各自Claude/Codex双PASS和精确签名本地提交，source-qualified；MTR-38已开工，三源码首版待真实验收/自身双审；尚未bump/push/tag/Release/C-D。Mega2 91=3723200、01=1385f58；Libra01=32287f7（上游141de9ab）。Libra LTR-10仍等H1/LIVE，FIX3/LTR00取消及旧FAIL/STOP不变。 历史登记： 当前 2026-10-20 08:43:28 UTC：H0 R5a真实Claude/Codex双PASS；MTR-91自身双PASS、签名+DCO本地提交`3723200180b914f20af7152968438009cac373fe`已核，source-qualified；MTR-01本轮A/B已核、in-progress/locally-accepted，自身ER05及两仓精确本地提交待取得，之后才放行MTR-38。91/01均未C/D完成；Libra已原生快进141de9ab，LTR-10仍等H1/LIVE。 历史： 历史：2026-10-17 15:42:03 UTC：MTR-70 为 done/complete（`35d5457`，tag `v0.42.32`，Docker run `39045161292` success）。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-72。历史：MTR-03 为 done/complete（`959c1e2`，tag `v0.42.31`，Docker run `39039264618` success）。FIX-MTR-03 已发布 v0.42.28（done/complete，AC-1 与 VER-5 未勾选）。FIX-MTR-01 已发布 v0.42.29（`1dbfb0c`，Docker run `39014885765` success，done/complete）。MTR-02 为 done/complete（`b537ced`，tag `v0.42.30`，Docker run `39034027014` success）。MTR-01 继承该 C/D，为 done/complete。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-03。LTR-01 未开工。不推送 Libra。历史 2026-10-16 现场：origin/main=`b784111` 已推送，`REL-MTR-FIX-01` 退出；FIX3 预算与 STOP 不重置；FIX-MTR-01 与 FIX-MTR-03 改为各自 independent patch；FIX-MTR-04 不再发布；恢复窗已执行，AC-1 未勾选。历史迁机WIP检查点（2026-10-15T17:46:23.821690+00:00）：FIX3测试候选已取得受控旧10秒RED101、新180秒GREEN1/1、原exact1/1、默认module8/8及fmt/strict0；C2具体DB机制仍未知，第四oracle归属仅具名本地有界推断。B8于17:13:38..17:13:42 UTC实际exit1（native diff --check因引用空行尾空格exit2），STOP保留、3诊断/8AB额度耗尽，不重跑/重置/晋升；FIX3 blocked/空，ownER05/G13最终准入及FIX4完整C/D未取得，formal0/95与0/26。仅拟向handoff/scoped-traces-20261016保存未发布工作；0.42.27/main/tag/Release不变。新机先核OID/签名、整合上游并重做ER01–03和失败后的计划/验收，换机不续用旧窗口或STOP；本机不续后卡。 | N/A（mega2原生） | [`plan-20261015.md`](plan-20261015.md)，[Libra对偶](../../../libra/docs/development/plan/plan-20261015.md) | 2026-10-19 17:28:17 UTC |

## 工程安全基线

以下项目不新增 PT 编号，而是全部移植与收尾工作进入实施和发布前必须持续满足的工程门禁。完成状态必须由代码、回归测试和故障注入证明，不能仅以文档或人工约定关闭。

| ID | 修复主题 | 优先级 | 当前判断 | 阻断范围 |
|---|---|---:|---|---|
| SB-01 | 消除生产路径残余 panic/unwrap | P1 | 协议 response builder、`repo.rs` 路径转换、config 残余加载路径等仍有未收敛点 | 协议、API、配置加载、全部服务可靠性 |
| SB-02 | 凭据与 secret 边界持续收敛 | P1 | 本仓 `mail.password`/SMTP 与 website-mail bearer 已移除；剩余 redaction 覆盖面、resolver 缓存失效语义、对象存储 / Redis / `[notification.webhook]` SecretRef 收敛 | vault、对象存储、webhook 渠道凭据 |
| SB-03 | 测试门禁、Docker 测试栈与 CI 可信度 | P1/P2 | 三道门禁与 docker-compose 栈运行良好；PT-01 最小真实 Git CLI + 双实例 dispatcher 基线已落地；SSH/LFS 完整矩阵与真多进程黑盒仍缺 | CI 稳定性、回归可信度与全部 PT 的验收承载 |

### SB-01：消除生产路径残余 panic/unwrap

#### 当前风险

- `src/contract/git_protocol/` 与 `src/ceres/protocol/` 的 response builder、in-memory reader、`repo.rs` 路径转换等仍有 `docs/refactoring/protocol.md` 记录的残余 `unwrap()`。
- 配置加载路径上仍有 `docs/refactoring/config.md` 记录的残余 `expect`/`panic` 待继续收敛。
- 移植新模块时若直接复制 Mega 目标项目的 `unwrap()`/`expect()`/`panic!()`，会扩大风险面。

#### 修复要求

- 生产代码不得新增未解释的 `unwrap()`、`expect()` 或 `panic!()`；确属不可失败逻辑必须有 `// INVARIANT:` 注释并在任务验收中说明（`docs/plan/plan-template.md` 使用规则）。
- 协议输入一律走 fallible parser；网络字节不得触发 panic、整数下溢或越界切片。
- 移植任务交付前对移植面运行 panic 审查，Mega 侧的 panic 点必须 Result 化后落地。

#### 完成判据

- protocol.md、config.md 记录的残余清单全部关闭或有最小范围豁免与可验证 invariant。
- 故障注入（malformed pkt-line、坏 TOML、损坏 HEAD、失效路径）均产生稳定、可操作的错误而不终止进程。

### SB-02：凭据与 secret 边界持续收敛

#### 当前风险

- 本仓 SMTP / `mail.password` 与 website-mail bearer 已移除（ADR-WA-08 / plan-20260919）；对象存储、Redis 与 `[notification.webhook]` 等凭据的 SecretRef / 脱敏覆盖仍须持续收敛。
- redaction/SecretString 覆盖面（对象存储 key、外部服务 URL、webhook secret）未完全扩展。
- 凭据变更时 resolver 缓存失效语义（`evict`/`evict_all` 的调用点）未逐点确认。

#### 修复要求

- 新移植的凭据消费端一律先落 SecretRef，不新增明文凭据配置项；禁止在本仓回退 SMTP/`[mail]`。
- 错误与日志路径的脱敏测试随功能同步交付（沿用 `integration_error_redaction` 模式）。
- 引导循环硬约束继续成立：数据库凭据永远不能是 vault SecretRef；对象存储/Redis 进 vault 之前必须完成 PT-10 的后置初始化。

#### 完成判据

- 本仓无 SMTP/`mail.password` / website-mail 配置面；webhook 与其它 secret 缺失时 fail-closed 且错误不回显值。
- 活进程脱敏门禁覆盖 DB、Redis、对象存储与 `[notification.webhook]` 等现行凭据类。

### SB-03：测试门禁、Docker 测试栈与 CI 可信度

#### 当前风险

- 三道门禁与 docker-compose 测试栈（postgres:15、redis:7、rustfs）运行良好，但真实 Git CLI 兼容矩阵、多进程 outbox claim 竞争矩阵、热加载黑盒用例未建立。
- 移植 orion 三件套将引入 ws、VM、构建产物等新基础设施，测试栈不扩展则验收无承载。

#### 修复要求

- 新增基础设施组件必须先定义测试双层（模块集成 + `bin/tests/` 黑盒）与 docker-compose 拓扑，再落地实现。
- 禁止手写 schema SQL；必须断言连接的是 PostgreSQL；镜像固定 tag；secret 只经 stdin。

#### 完成判据

- PT-04 的真实 CLI 矩阵、PT-09 的多实例矩阵、PT-06/07 的 ws/构建链路基线用例进入 CI 可运行形态（基建承载由 PT-01 交付）。

## PT-01：集成测试基建扩展与统一

### 移植问题

当前集成测试基建（docker-compose 测试栈 postgres:15/redis:7/mailpit、`bin/tests/` 黑盒、模块集成测试、CI `config-validation.yml`）只覆盖 config/vault/mail/notification 链路。后续 PT 将引入真实 Git CLI 协议矩阵、orion ws 服务、多实例 outbox 竞争、热加载黑盒等新形态负载，现有拓扑与 harness 无承载；不先统一基建，每个 PT 会各自发明测试脚手架，重复、不可比且无法进入 CI。

### 目标范围

- 统一测试双层规范（模块集成测试 + `bin/tests/` 黑盒）与 fixture 生命周期管理，新组件按规范接入。
- docker-compose 测试栈拓扑扩展机制：新增服务（Git CLI runner、orion-server、fake webhook receiver 等）有标准登记方式；固定端口、固定镜像 tag、secret 只经 stdin 的纪律不变。
- 交付真实 Git CLI smoke 最小矩阵（HTTP 通道 clone/push），作为拓扑扩展的示范负载。
- 热加载黑盒用例、多实例 outbox claim 竞争基线用例进入 CI 可运行形态。
- `docs/refactoring/integration.md` 的 P2 清单逐项落地或书面关闭。

### 非目标

- 不追求覆盖率数字；本 PT 只为后续 PT 提供验收承载。
- 不重写既有测试；只扩展拓扑与规范。
- Git 协议专项矩阵的完整用例面与 Mega `tests/` 夹具的审计移植属 PT-04。

### 完成判据

- 新服务接入测试栈有标准流程，并以 Git CLI runner 为示范落地。
- HTTP 通道真实 Git CLI 最小矩阵、热加载黑盒、多实例竞争基线用例 CI 绿。
- integration.md P2 清单全部有"落地 / 关闭（含理由）"结论。

### 审计证据、真实缺口与提升条件

- **mega2 现状证据（已完成，2026-07-30）**：`docker/docker-compose.test.yml`、`bin/tests/integration_vault.rs`、`bin/tests/integration_git_cli.rs`、`.github/workflows/config-validation.yml` / `git-protocol-smoke.yml` 已复核；`docs/refactoring/integration.md` P2 清单为 **扇出/热加载/`config init` 已落地**，缺口书面关闭（`DEFER-IT-07`/`DEFER-IT-08`/`DEFER-IT-10`）；git-cli runner、HTTP 最小矩阵、并发 dispatcher 基线与 CI 执行/触发面已由 `plan-20260727.md` 交付。IT-03 / IT-04 / IT-11 D 组已绿（`validate-config` [30466831819](https://github.com/gitmono-dev/mega2/actions/runs/30466831819)；`git-protocol-smoke` [30479301244](https://github.com/gitmono-dev/mega2/actions/runs/30479301244)）。
- **Mega 证据**：`mega/tests/` 协议夹具存在（专项审计移植归 PT-04）；`mega/orion*` 的 ws/构建链路测试形态在 mega2 无承载（归 PT-06/07）。
- **完成判据对照（`:202-204`）**：① 新服务接入有标准流程且以 Git CLI runner 示范落地 —— 满足（`test-infra.md` + compose `git-cli`）；② HTTP 通道真实 Git CLI 最小矩阵、热加载黑盒、多实例竞争基线用例本地绿、已接入 CI，且远端 CI 绿（IT-03/IT-04/IT-11 D 组）—— 满足；③ integration.md P2 清单全部有「落地 / 书面关闭（含理由与承接编号）」结论 —— 满足（无「延后」）。
- **风险与边界**：测试栈膨胀会拖慢 CI；新服务必须可选启用（`profiles`），本地单元测试路径不受影响。SSH/LFS 完整矩阵与真多进程黑盒仍分别属 PT-04 / PT-09。

### 依赖与顺序

PT-01 不依赖其他 PT，是全部后续 PT 的验收承载：PT-04（协议矩阵）、PT-06/PT-07（orion 链路）、PT-09（多实例矩阵）的验收依赖本 PT 的拓扑扩展。

---

## PT-02：已移植模块 Mega 基线追平与持续同步

### 移植问题

mega2 的 callisto、jupiter、ceres、mono 对应面最后系统同步基于 Mega `#2129`（v0.1.50 同步分析报告），Mega 已推进到 `#2169`。在漂移窗口上直接实施 PT-03 至 PT-08，会把"移植缺口"与"Mega 新变更"混在一起，导致锚点失效和重复劳动。

### 目标范围

- 用 libra 核对 Mega `#2130..#2169` 的全部提交，按模块分类：callisto 实体、jupiter storage/migration、ceres、mono API/命令、vault、saturn、api-model、clients。
- 对每个已移植模块给出"吸收 / 明确排除（含理由）/ 转 PT-xx 承接"的三选一结论。
- 建立持续同步机制：每次审计记录 Mega revision 与漂移结论，写回本文"审计快照"章节。
- 漂移追平不得引入与 mega2 架构改进冲突的回流（原则 2、8）。

### 非目标

- 不追赶 Mega 的发布节奏；只追平与已移植模块相关的变更。
- 不在本 PT 内实施 orion 三件套等新模块移植（属 PT-06/07/08）。
- 不处理 moon、scripts、docker 等非 Rust 资产。

### 完成判据

- Mega `#2130..#2169` 每个提交都有书面归类结论；被吸收的变更已合入并通过三门验收。
- callisto 实体与 migration 与 Mega 当前 schema 一致，或差异有书面豁免理由。
- 本文"审计快照"更新到吸收完成时的 Mega revision，并注明下一次核对基线。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`main` @ `3d22823`（含 #2169）；#2130–#2169 区间内容未在 mega2 侧归类。优先审计 #2138/#2139 的 ceres application/transport/bus 重构、#2145 账户审批、#2147 Cedar 管理、#2152 缺失对象错误处理，以及 #2143/#2146/#2150/#2160/#2163 的 Orion provisioning、VM、磁盘和排队语义；#2165..#2169 的 identity/Cedar reviewer 域（Cedar reviewer 解析、campsite_user_id、admin 检查、CLA 签名）与 #2145/#2147 同域，一并纳入优先审计。
- **mega2 现状证据**：v0.1.50 提交说明明确"Read-only analysis of upstream mega (HEAD #2129)"；此后无同步记录。
- **最小可验证第一阶段**：先产出逐提交归类表（纯审计，不改代码），再按模块分批吸收；第一批只处理 callisto/jupiter（数据层漂移风险最高）。
- **风险与边界**：漂移追平期间若发现 Mega 侧重构与 mega2 架构决策冲突，停下来记 ADR，不做静默折中。

### 依赖与顺序

PT-02 不依赖其他 PT，是 PT-03、PT-05、PT-06 实施前的推荐前置（在漂移窗口上开工的 PT 必须先自行完成相关模块的局部追平）。

---

## PT-03：Git 协议兼容性与 LFS 收尾

### 移植问题

Git smart HTTP/SSH 协议已完成输入 panic 止血、`info/refs` 严格化、fallible pkt-line parser、SSH exec parser、receive-pack 按 flush-pkt 分割、SSH per-channel state、HTTP/SSH 认证上下文、delete-only push、capability truth table 及 LFS 路径/认证边界加固。核心缺口收敛为：协议路径仍缓冲完整 body/channel、repo/path 级 push ACL、并发 receive-pack 与完整真实客户端故障矩阵。

### 目标范围

- streaming pkt-line reader：HTTP/SSH 协议路径不再缓冲完整 body/channel。
- repo/path 级 push ACL 与 Cedar deny 的协议层回归矩阵；认证后不得退化为默认 permit-all 的隐性放行。
- 并发 receive-pack、`git push --delete`、LFS hybrid 与 missing-repo 的真实客户端故障矩阵。
- capability truth table 的持续维护：新 capability 仅在 advertise/parse/act-on/test 四列一致后声明；不重新声明尚未证明原子性的 `atomic`。
- 残余 `unwrap()` 收敛（与 SB-01 联动）。

### 非目标

- 不承诺 protocol v2、`deepen`/shallow clone、partial clone、pure SSH LFS（protocol.md 阶段 6，P3 另行决策）。
- 不为了对齐 Mega 而重新声明未实现且未测试的 capability。

### 完成判据

- capability truth table 无 ⚠️/❌ 项，或每项有书面豁免；大 payload 在受限内存下 streaming 处理。
- SSH publickey 认证后所有协议操作的 commit binding 携带正确身份，有回归测试。
- repo/path push deny、`git push --delete`、LFS hybrid、并发 receive-pack 的故障注入矩阵通过。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/mono/src/git_protocol/`、`mega/ceres/src/transport/` 为移植基线；mega2 已在正确性上局部超过 Mega（capability 诚实化），不回退。
- **mega2 现状证据**：`src/contract/git_protocol/{http,ssh}.rs`、`src/ceres/protocol/smart.rs` 的止血切片已合入；protocol.md 阶段 3–6 的 streaming 仍未完成。LFS FastCDC Media（`--features fastcdc`）、receive-pack per-command status / CAS delete、导入 filepath 批处理已由 [`plan-20260901.md`](plan-20260901.md) 交付，不关闭本 PT 的 streaming / 完整故障矩阵缺口。
- **最小可验证第一阶段**：依 `plan-20260803.md` 先补 HTTP pull / anonymous_access=false / LFS / SSH 真实客户端矩阵，再做 streaming 重构（性能与内存）或 ACL 收敛。
- **风险与边界**：streaming 重构改变热路径内存模型，必须带基准与回退方案；依赖 PT-04 的 CLI 矩阵做防回归。

### 依赖与顺序

PT-03 的 streaming/ACL 行为切片应以 PT-04 的真实 CLI 矩阵为验收承载。开工前完成 PT-02 对 git_protocol/ceres 相关提交的局部追平。

---

## PT-04：Git 协议集成测试夹具与真实 CLI 兼容矩阵

### 移植问题

Mega `tests/` 提供协议/对象层集成测试夹具（`data/`、`diff/`、`objects/`、`refs/`、`scripts/`），mega2 尚未系统移植；`test/project/` 与 `bin/tests/` 不覆盖该层。HTTP 最小真实 CLI 矩阵已存在，但 HTTP pull、匿名读关闭、HTTP LFS、cargo-native SSH、夹具许可/生成方式和 CI 归属尚待补齐，PT-03 后续改动仍缺完整防回归网。

### 目标范围

- 审计 Mega `tests/` 夹具的内容、许可与生成方式，决定移植、重建或替代。
- 建立真实 Git CLI smoke 矩阵：HTTP/SSH/LFS 三通道的 clone/fetch/pull/push/delete 与关键拒绝用例（protocol.md 已列出建议清单）。
- 矩阵进入 CI 可运行形态（沿用 docker-compose 测试栈纪律：固定端口、固定 tag、无手写 schema）。

### 非目标

- 不追求 Git 官方测试套件级覆盖率（属另一量级的兼容治理）。
- 不把夹具数据直接复制进仓库而不核对其许可与体积。

### 完成判据

- 真实 Git CLI 对 mega2 服务完成 HTTP/SSH/LFS 三通道正路径与关键故障路径用例，CI 绿。
- 夹具或重建数据有来源记录；新增协议能力必须同步矩阵用例（门禁化）。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/tests/{data,diff,objects,refs,scripts}` 存在且被 Mega 测试使用。
- **mega2 现状证据**：`test/project/` 为业务集成测试夹具；`bin/tests/` 已含 `integration_git_cli`（HTTP 最小矩阵 + 字面 `pull` + 匿名读关闭拒绝）、`integration_git_lfs`（HTTP LFS 往返）、`integration_git_ssh`（cargo-native SSH clone/pull/push 与坏密钥拒绝），三 target 已进入 `config-validation.yml` 与 `git-protocol-smoke.yml` 执行面；Mega 夹具审计 NO-GO 落盘于 `docs/refactoring/mega-git-fixtures-audit.md`（`DEFER-GM-03`）。
- **最小可验证第一阶段**：已由 `plan-20260803.md` 交付（HTTP `pull`、`anonymous_access=false` 拒绝、HTTP LFS、cargo-native SSH 与 Mega fixture go/no-go）；「三通道正路径与关键故障路径 CI 绿」已满足，夹具经审计记录来源/许可后决策 NO-GO（`DEFER-GM-03`）；长期完成判据未全部关闭（`DEFER-GM-01/02/04/05` follow-up），本 PT 维持实施中。
- **风险与边界**：SSH 测试固定为 cargo-native self-start，不新增 compose sshd；LFS 依赖对象存储后端，优先 local FS 后端。

### 依赖与顺序

PT-04 紧随 PT-01 启动（拓扑与最小矩阵承载来自 PT-01），其完整矩阵是 PT-03 后续切片的验收承载；与 PT-02 无冲突，可并行。

---

## PT-05：ceres/bus 事件总线与 infra 基础设施归位

### 移植问题

Mega `ceres/src/bus/`（`TransportRuntime`、`TransportEvent`、`ApplicationEventHandler`）与 `ceres/src/infra/`（cache、pack_decode、pack_stream）未作为模块移植。cache 已并入 `src/ceres/api_service/cache.rs`，pack 相关逻辑散落在 `src/ceres/pack/` 与 `src/ceres/protocol/smart.rs`。orion 构建链路（PT-06/07）与 build 完成触发器（PT-09）在 Mega 中均经过事件总线通信，缺失总线会迫使后续 PT 发明临时通道。

### 目标范围

- 审计 `ceres/bus` 在 Mega 中的全部生产者/消费者，判定 mega2 需要的事件面（构建触发、构建完成、传输事件）。
- 决定归位形态：按 `src/contract/` 边界规范落入合适模块，或论证后明确不移植（状态转"不采纳"并记录理由）。
- `infra` 的 pack_decode/pack_stream 逻辑归位：消除散落重复，落点符合现有 ceres 模块划分。

### 非目标

- 不引入 Mega 未使用的通用事件框架；只移植有真实消费者的事件面。
- 不在本 PT 内接入 orion（消费者接入属 PT-06/07/09）。

### 完成判据

- 事件总线归位（或书面不采纳结论）合入；生产者/消费者清单与 Mega 侧一一对应或有豁免。
- pack_decode/pack_stream 无散落重复实现；三门验收通过。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/ceres/src/bus/`、`mega/ceres/src/infra/` 目录与引用图。
- **mega2 现状证据**：`src/ceres/` 模块清单无 bus/infra；`build_trigger` 已存在但事件通道缺失。
- **最小可验证第一阶段**：纯审计——画出 Mega 侧事件生产者/消费者图，给出归位 ADR；代码落地放第二阶段。
- **风险与边界**：事件总线是 PT-06 通信形态的前置决策，拖延会阻塞构建链路设计。

### 依赖与顺序

PT-05 的审计结论（采用 bus 与否）是 PT-06 的前置；开工前完成 PT-02 对 ceres 相关提交的局部追平。

---

## PT-06：Orion Server 构建控制面移植

### 移植问题

Mega `orion-server`（api、buck2、log、model、repository、scheduler、service、server）是构建任务控制面：接收 TaskBuildRequest、调度任务、收集日志、管理产物。mega2 只有消费侧——`buck_router`/`artifacts_router`/`build_trigger_router` API 面与 bellatrix 客户端——构建任务无处投递。这是"mega 完全移植"最大的整体缺口之一。

### 目标范围

- 移植 orion-server 的 HTTP API、任务模型、调度、日志收集与产物管理，落点遵循 `src/contract/` 与 jupiter 分层规范。
- 移植 mono 侧 `orion_runner_router`（runner 上报/注册接口），补齐 `src/api/router/` 缺失面。
- 实体/迁移按 `AGENTS.md` 流程进入 `src/callisto/` 与 `src/jupiter/migration/`；DB schema 与 Mega 对齐或书面豁免。
- 与 bellatrix（客户端侧）完成端到端联通：dispatch → server 接收 → 状态可查。

### 非目标

- 不移植 runner 执行逻辑（PT-07）与 VM 调度（PT-08）。
- 不改变 mega2 既有 buck/artifacts API 的公开契约（除非书面 ADR）。

### 完成判据

- bellatrix dispatch 的构建任务在 orion-server 侧可接收、可调度、可查询状态与日志。
- docker-compose 测试栈扩展出 orion-server 拓扑（拓扑机制来自 PT-01），`bin/tests/` 黑盒用例覆盖 dispatch→接收正路径。
- OpenAPI schema、错误码文档、迁移同步；三门验收通过。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/orion-server/` 完整 crate；`mega/mono/src/api` 的 orion_runner_router；`mega/clients/orion-client` 的完整 API 面。
- **mega2 现状证据**：`src/bellatrix/` 仅 build dispatch 路径；API state 无 orion-server 相关字段；`src/api/router/` 无 orion_runner_router。
- **最小可验证第一阶段**：任务模型 + 接收 API + 内存/DB 调度最小环，bellatrix 正路径联通；日志与产物管理第二阶段。
- **风险与边界**：通信形态（直发 HTTP vs 事件总线）必须先等 PT-05 结论；schema 对齐需 PT-02 先行。

### 依赖与顺序

依赖 PT-05（通信形态决策）与 PT-02（ceres/callisto 追平）；验收拓扑依赖 PT-01；是 PT-07 的前置。

---

## PT-07：Orion 构建执行 Agent 移植

### 移植问题

Mega `orion` 是跑在构建机上的执行 agent：经 ws 与 orion-server 通信，执行 buck2 构建（`buck_controller`）、管理磁盘与 repo 缓存。mega2 完全缺失 runner 侧，构建链路即使有 server 也无法执行。

### 目标范围

- 移植 orion agent 的 ws 客户端、任务接收、 buck_controller、disk/repo 缓存管理。
- 与 PT-06 的 server 完成 ws 协议联通：注册、接任务、上报状态与日志。
- runner 侧配置进入 `src/config/` 体系（LoadMode、校验、脱敏纪律不变）。

### 非目标

- 不移植 QEMU/VM 弹性承载（PT-08）；runner 先以常驻进程形态交付。
- 不实现 Mega 也没有的新调度策略。

### 完成判据

- 端到端：bellatrix dispatch → orion-server → orion agent 执行（可用 fake/stub buck2）→ 状态与日志回传可查。
- ws 断连重连、任务抢占、磁盘水位等故障注入用例通过；三门验收通过。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/orion/src/{ws,buck_controller,disk,repo,api}`。
- **mega2 现状证据**：无任何 runner 对应物。
- **最小可验证第一阶段**：ws 注册 + 接收任务 + stub 执行 + 状态回传，buck_controller 第二阶段接入真实 buck2。
- **风险与边界**：buck2 依赖与构建机环境差异大，stub 先行可避免环境阻塞；ws 协议契约以 PT-06 落地为准。

### 依赖与顺序

依赖 PT-06（server 与 ws 契约）；是 PT-08 的前置。

---

## PT-08：Orion Scheduler、VM 弹性调度与客户端 API 面补齐

### 移植问题

Mega `orion-scheduler`（QEMU VM 池、webhook、keep_alive、vm_cleanup、orion_deployer）与 `clients/orion-scheduler-client` 未移植；bellatrix 也只覆盖 build dispatch，`OrionBuildClient` 完整 API 面未核对。构建链路最后一段（弹性承载与完整客户端契约）缺失。

### 目标范围

- 核对并补齐 `OrionBuildClient` 完整 API 面（对照 `mega/clients/orion-client`）。
- 移植 orion-scheduler-client；评估并决定 orion-scheduler（QEMU VM 池）的移植或书面不采纳（运维环境依赖重）。
- 若移植 scheduler：VM 生命周期、webhook、keep_alive/cleanup 进入 mega2 模块体系与配置体系。

### 非目标

- 不承诺与特定虚拟化平台绑定的新能力；超出 Mega 的调度智能化不在范围内。
- 若决策不移植 QEMU 层，必须用等价的 runner 供给方案（如静态 runner 池文档）闭合构建链路，不留隐性缺口。

### 完成判据

- bellatrix/客户端 API 面与 Mega 对齐（或有书面豁免清单）。
- scheduler 移植或"不采纳 + 替代供给方案"结论合入；采纳则 webhook 触发建 VM、keep_alive、cleanup 的故障注入用例通过。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/orion-scheduler/`、`mega/clients/orion-scheduler-client/`、`mega/clients/orion-client/`。
- **mega2 现状证据**：`src/bellatrix/` 部分覆盖；API state 已移除 orion_scheduler_client 字段（移植时需重新引入）。
- **最小可验证第一阶段**：客户端 API 面核对与补齐（纯客户端，无基础设施依赖）。
- **风险与边界**：QEMU 依赖具体运维环境，移植决策必须显式，不能默认顺延。

### 依赖与顺序

客户端补齐可与 PT-06 并行；scheduler 移植依赖 PT-07（先有可承载的 runner）。

---

## PT-09：通知与邮件能力对齐收尾

### 移植问题

`src/notification/` 的出站面已收敛为 **generic webhook**（`[notification.webhook]`，凭据经 SecretRef）。SMTP、Slack、in-app inbox 与 website-mail 客户端已拆除（[`plan-20260919.md`](plan-20260919.md)；墓碑 `docs/refactoring/website-mail.md`）。用户偏好只保留 `enabled` + per-event。本仓 compose 数据面不启动 SMTP 捕获服务。长期收尾缺口收敛为 build 完成触发器接入点、次渠道 retry 与真实多进程/多实例黑盒矩阵（对象改为 webhook）。

### 目标范围

- build 完成触发器：接入点依赖 PT-05/PT-06 的事件面，先有事件后接触发器。
- 维持「本仓不发信」边界：禁止回退本仓 SMTP 或 website-mail 客户端。
- 多实例 webhook 竞争、背压、重复发送/丢失边界的黑盒矩阵。

### 非目标

- 不回流 Mega 的旧 notification/email 实现（原则 8）；不在本仓重建 SMTP outbox。
- 不把 CL/issue 事件编排整体迁到 website（仅投递归属 website）。

### 完成判据

- 文档与 IT 持续无本仓 SMTP 投递门；出站只剩 `[notification.webhook]`；多实例矩阵 CI 绿。
- build 完成通知不绕过事件面；次渠道 retry、背压与重复/丢失边界有真实多实例证据。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/mono/src/notification/` 为基线对照；slack 渠道参考 campsite `slack.ts`。
- **mega2 现状证据**：`src/notification/` 与 `docs/refactoring/{notification,mail,website-mail,test-infra}.md`；出站拆除见 [`plan-20260919.md`](plan-20260919.md)；邮件退场史见 [`plan-20260731.md`](plan-20260731.md) MN-01..MN-06。
- **最小可验证第一阶段**：在 PT-05/PT-06 事件语义冻结后，为 build completed 接入既有通知编排；不得新建旁路投递机制。
- **风险与边界**：build 触发器不得绕过事件面临时硬编码进 PT-06；未配置 webhook 时通知面静默。

### 依赖与顺序

build 完成触发器依赖 PT-05（事件面）与 PT-06（构建事件源）；多实例矩阵的测试栈承载依赖 PT-01；邮件投递契约与 website 内部 API 已由 [`plan-20260802.md`](plan-20260802.md) / `website-mail.md` 关闭 DEP-06。

---

## PT-10：配置体系与 SecretRef 收尾

### 移植问题

`src/config/` 已大幅超越 Mega `common/config`。**`[oauth]` 已由 [`plan-20260731.md`](plan-20260731.md) 落地**（`website_api_base_url` / session cookie 名 / CORS；消费者为 website Better Auth 会话路径，见 `docs/refactoring/website-auth.md`）；对象存储与 Redis 的 vault `SecretRef` 已通过 DB-only vault bootstrap 落地，跨 source/profile diagnostics 矩阵和首批热加载黑盒也已存在。剩余收尾是把真实消费端订阅与跨 await 生命周期逐点审计，持续扩展而非重做启动顺序。

### 目标范围

- 热加载：更多真实消费端订阅接入，"取出 Arc 后跨 await 持有"语义逐点确认。
- 对新增配置消费者持续扩展 profile/跨 source diagnostics；残余 `expect`/`panic` 收敛（SB-01 联动）。
- 维持 `[oauth]` 与 website-mail 客户端配置为活配置（无死键回流）。

### 非目标

- 不改变引导循环硬约束（DB 凭据永远不进 vault）。
- 不在本 PT 内做配置格式换代（TOML 保持）。
- 不重做 `[oauth]` 接线（已由 plan-20260731 AU 链交付）。

### 完成判据

- 热加载订阅清单与回滚语义有测试覆盖；diagnostics 矩阵进入 CI。
- 对象存储/Redis SecretRef 与 `[oauth]` 保持有消费者，启动顺序不破。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/common/src/config` 为基线对照（mega2 已是超集）。
- **mega2 现状证据**：`src/config/` 与 `docs/refactoring/{config,website-auth}.md`；`[oauth]` 落地见 [`plan-20260731.md`](plan-20260731.md) AU-02..AU-05。
- **最小可验证第一阶段**：列出当前热加载订阅者及其 Arc 跨 await 语义，以一个未覆盖的真实消费者补充回滚/重载测试。
- **风险与边界**：不得破坏现有 DB-only vault bootstrap；数据库凭据仍永远不进入 vault。

### 依赖与顺序

各切片相互独立，可与 PT-02..PT-08 并行；新增凭据消费者继续遵守 post-vault 解析与 SB-02 脱敏约束。

---

## PT-11：Vault 安全工程收尾

### 移植问题

Vault 集成层 A–J 阶段可交付子集已完成：file 持久化 audit sink、可选 fail-closed 审计、unseal share rekey 与 backup/restore 均已存在。工程收尾仍在：KEK 轮换无内建原语、异地/HTTP audit sink、root recovery 材料外置，以及 `core_key.json` 与 vault 表的格式版本策略。

### 目标范围

- KEK 轮换专项：`init()` 后 KEK 可变，旧 unseal 分片彻底失效路径闭合。
- 审计：在既有 tracing/file sink 之外补异地 sink，并维持可选 fail-closed 写入策略。
- root recovery：root 恢复材料外置（root recovery token 或外部托管）与本地 key 文件的安全移除路径。
- `core_key.json` 与 vault 表的格式版本/兼容迁移策略落地。
- 可选评估：PKI 原生 ssh/pgp 证书替代 KV 裸私钥存储（单独立项后承接）。

### 非目标

- 部署侧交付（KMS、systemd `LoadCredential`、K8s Secret volume、备份演练）属运维职责，不进本 PT 代码范围。
- 不改变威胁模型声明：自动解封的 Shamir 分片不防护磁盘读取攻击者，不夸大安全收益。

### 完成判据

- KEK 轮换后旧分片不可再解封，有故障注入与恢复手册同步。
- 异地审计 sink 配置化，fail-closed 策略有测试；root 恢复材料可外置且本地移除路径有演练记录。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/vault/` 为移植基线对照（mega2 已在集成层加固，超出 Mega）。
- **mega2 现状证据**：形态为 crates.io `libvault` 0.3.0 + `src/contract/vault/integration/vault_core.rs`（vendored `src/vault/` 已于 2026-08-21 删除，[`plan-20260820.md`](plan-20260820.md) ADR-VLT-01）。`docs/refactoring/vault.md` 已复核；`tracing`/`file` sink、`fail_closed`、backup/restore 和 unseal rekey 已交付。只读审计语义（UN-31）在迁移后须保留于集成层，不得弱化。
- **最小可验证第一阶段**：格式版本策略（低风险、纯工程）；KEK 轮换需专项设计后承接。依赖形态迁移已走 go 路径落地，因此 PT-11 触及 seal/core 的卡一律按 `libvault` crate + `src/contract/vault/` 刷新锚点，不再引用 `src/vault/`。
- **风险与边界**：KEK 轮换触碰 seal 核心，必须配恢复手册与故障注入，不得赶工；不得为对齐 Mega 目录而回流 vendored fork（原则 2）。

### 依赖与顺序

各切片相互独立，可与其他 PT 并行；PT-09 新渠道凭据依赖本 PT 既有 SecretRef 能力（已满足）。依赖形态迁移见 [`plan-20260820.md`](plan-20260820.md)，不阻塞本 PT 的非 seal 切片，但 seal/core 相关切片应避开与 VLT-* 写集冲突的窗口。

---

## PT-12：前端与账户系统一致性（website `apps/next-app` ↔ Mega moon+campsite）

### 移植问题

mega2 的后端能力（CL、issue、评审、通知、构建等）必须有前端与账户系统承载。Mega 目标项目的前端体系是 moon（Next.js）+ campsite（Rails，账户/后端配对）；mega2 的对应物是 monoui 仓库 `mega2` 分支的 `apps/next-app`。两套前端体系独立演进会产生双向漂移：mega2 新增或变更的 API/账户行为在 `apps/next-app` 无承载，或 `apps/next-app` 缺少 moon 已有功能，用户侧表现为两侧产品能力不一致。

**已由 [`plan-20260731.md`](plan-20260731.md) 落地（非全量对照）：** 浏览器会话信任路径（cookie → website `get-session` → `LoginUser`）、`[oauth]` 接线，以及 `mega2-it` 同栈 `website-next` + `integration_website_auth`。本仓 Campsite 风格 chat/Notes 产品面已退场（产品能力留在 website）。**全量** moon↔`apps/next-app` 功能对照审计仍为候选（DEP-03）。

### 目标范围

- 建立双前端功能对照基线：`apps/next-app`（monoui `mega2` 分支 pinned revision）与 `mega/moon/` + campsite 的功能清单与差异表，每项差异给出"追平 / 书面豁免（含理由）"结论。
- 一致性门禁化：mega2 每个涉及公开 API、账户、认证行为的 PT/日期计划，任务卡必须包含前端影响评估（apps/next-app 是否需要联动改动）。
- 账户系统语义对齐：在已落地的 website Better Auth 信任路径之上，继续做与 campsite/moon 的功能语义对照，差异书面化。
- 追平机制：差异追平按功能域拆分进入日期计划；前端代码演进在 monoui 仓库进行，mega2 侧只维护契约（OpenAPI/DTO）与对照表。

### 非目标

- 不把 moon 或 `apps/next-app` 的代码移植进 mega2 仓库；前端在各自仓库演进。
- 不统一两侧技术栈与 UI 实现；一致性指功能语义与用户可见能力，不是像素级一致。
- 不在本 PT 内重复实现会话信任路径 / compose 同栈 IT（已由 plan-20260731 交付）。

### 完成判据

- 功能对照表建立并写回本仓 `docs/`（含 pinned revision 与核对日期），后续审计可持续刷新。
- 账户/认证语义差异全部书面化，无未记录的隐性行为差异。
- 原则 11 的门禁生效：抽查若干涉及公开行为的日期计划，其任务卡均含前端影响评估。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：`mega/moon/`（Next.js 主前端）；campsite（`/run/media/eli/data/gitmono/campsite`，Rails：`api/`、`apps/`）为 moon 的账户/后端配对；#2165..#2169 的 identity/Cedar reviewer 域（Cedar reviewer 解析、campsite_user_id、admin 检查、CLA 签名）与 #2145/#2147 同属账户/认证语义，纳入全量对照范围。
- **mega2 现状证据**：website Better Auth 会话与同栈 IT 见 `docs/refactoring/website-auth.md`、`test-infra.md`、[`plan-20260731.md`](plan-20260731.md) AU/ITW；**全量功能对照表仍不存在**。
- **最小可验证第一阶段**：纯审计——产出 apps/next-app ↔ moon+campsite 功能对照表与账户语义差异清单（不改代码），据此把 PT-12 从"候选"提升为"已验证"并排期首批追平切片。
- **风险与边界**：对照表一旦落后即失效，必须随每次审计刷新；前端改动跨仓库，联动发布顺序（先后端兼容窗口、后前端切换）必须在各日期计划中显式声明。

### 依赖与顺序

会话信任路径与同栈 IT 已具备（plan-20260731）；全量对照审计可立即启动。API 面追平（PT-02）与 orion API（PT-06）落地时会扩大前端一致性面，相关功能对照须随之刷新。

---

## PT-13：storage-only 统一推送密文（HTTP + SSH receive-pack）

mega2 原生能力，不是 Mega 移植。承接 [`plan-20260908.md`](plan-20260908.md) `DEFER-SP-02`（重启条件：产品明确要求 SSH 推送）。本文只冻结方案与进入日期计划的条件；接线、测试与发布必须另开 `plan-YYYYMMDD.md`。

### 现状缺口

Storage-only（显式 `git.push_auth`）已经有**一份**静态密文表 `[[git.push_tokens]]`：

| 已交付 | 证据 |
|---|---|
| 常量时间查找 `lookup_push_token`；命中身份 = `name`；username 不参与比对 | `src/contract/git_protocol/mod.rs` |
| HTTP 出示：Bearer 或 Basic **password**（username 丢弃） | `src/contract/git_protocol/http.rs` `git_http_auth` |
| SSH 读出示：`auth_password` 的 password = 同一密文 | `src/contract/git_protocol/ssh.rs`；ADR-SP-01 |
| 写授权：`check_push_permission` → `apply_push_auth_gate` → `token_covers_repo`（`paths` 组件边界） | 同 `mod.rs`；trunk 下 Cedar `off`，token `paths` 是唯一写 ACL |
| LFS 批/锁、产品 API 写、OCI `/v2` 复用同一查找 | `lfs_router.rs`、`api_write_auth.rs`、`oci/auth.rs` |
| SSH receive-pack **恒关**：`ssh_receive_pack_enabled()` = `!storage_only() && ssh_receive_pack != Some(false)`；启动强制 `ssh_receive_pack=false` | TP-20；`src/config/model.rs`；`validate.rs` |

TP-20 关掉 SSH 写，是因为当时静态 token **只做了 git-over-HTTP**，SSH 路径仍要求 UserStorage 已认证用户。若在没有 token 闸的情况下挂着 receive-pack，会留下「无凭据要求的隐性推送通道」。[`plan-20260908.md`](plan-20260908.md) 只把 SSH **读**对齐 HTTP，并写明不打开 SSH 推送。

因此今天：**同一枚密文已经能 HTTP 推 + SSH 拉**；SSH 推即使 password 正确也回 `SSH receive-pack is disabled`。缺的是「同一闸门覆盖 SSH 写」，不是第二套密钥。

### 选定方案

**一枚密文、两套传输、同一写闸。** 不新增 token 表，不把 username 当密钥，不把 Agent ingest token 或 review 公钥混进来。

1. **唯一密钥**仍是 `[[git.push_tokens]].token`。`name` 只做命中后的 `authenticated_user`；commit author/committer 仍是自声明 provenance。
2. **出示约定不变**：HTTP = Bearer 或 Basic password；SSH = `auth_password` 的 password。两端都忽略协议层 username。
3. **查找与写闸不变**：两侧命中后都 `set_authenticated_user(name)`，receive-pack 都走现有 `check_push_permission`（无凭据 401，`paths` 不覆盖 403）。禁止为 SSH 再写一套路径匹配。
4. **打开 SSH 写的形态闸（fail-closed）**：
   - `push_auth=token` **且** 配置显式 `ssh_receive_pack=true` → `ssh_receive_pack_enabled()` 为真；storage-only 启动不再因为这项是 `true` 而拒绝。
   - `push_auth=none` **禁止** `ssh_receive_pack=true`（启动拒绝）。匿名写继续只出现在运维显式打开的 HTTP 面上（`deploy-trunk.md` §3）。
   - `push_auth=token` 且省略 / `false` → SSH 写仍关（与今日行为兼容；SSH 推送是 opt-in，不是 token 模式的默认副作用）。
   - review（省略 `push_auth`）语义不变：仍走 UserStorage 公钥；不得因本方案成功放行 `auth_none`。
5. **`ssh_receive_pack_enabled()` 目标谓词**（日期计划开工日刷新锚点后再改代码）：

   ```text
   review:     ssh_receive_pack != Some(false)          # 与今日相同
   token+true: true
   token+false/省略: false
   none:       false（且 true 为配置错误）
   ```

6. **客户端**：同一 `SECRET`。HTTP `http://x:SECRET@host/path.git` 或 `Authorization: Bearer SECRET`；SSH `SSH_ASKPASS` / `sshpass`，password=`SECRET`，再 `git push`。成功条件与 HTTP 相同：只接受 `refs/heads/main`，经 `MonoWriteQueue` 落地。
7. **LFS**：Git 对象可经 SSH 推；LFS 仍走 hybrid HTTP，继续用同一密文。pure `git-lfs-transfer` 仍是 `DEFER-SP-03` / `DEFER-LF-01`，不在本方案。
8. **配置挂公钥**（`DEFER-SP-01`）不是本方案。本方案的 SSH 推送凭据就是 push token 密文，不是第二把钥匙。

### 目标范围

- 修订 `ssh_receive_pack_enabled` 与 `validate_trunk_config_surface`，落实上一节形态闸。
- SSH exec `git-receive-pack` 在 token 命中后进入与 HTTP 相同的 `finalize_receive_pack` / B0–B3，而不是通道级直接 `disabled`。
- 进程级 IT：同一密文 HTTP push 与 SSH push 都前进 `/project` 的 `main`；坏密文拒；`paths` 越权拒；`push_auth=none` 即使有人配置 `ssh_receive_pack=true` 也启动失败；review 公钥推送回归仍绿。
- 同步 `docs/deploy-trunk.md`、`docs/refactoring/protocol.md`、`docs/user-guide.zh.md`（若提及 SSH 边界）、配置样例注释与 `README.zh.md` storage-only 段。
- 关闭 `DEFER-SP-02`（日期计划完成时回写该行承接位置）。

### 非目标

- 不在 `push_auth=none` 上开 SSH receive-pack。
- 不把 SSH 写做成默认（token 模式仍须显式 `ssh_receive_pack=true`）。
- 不引入 `ssh_public_keys`、不查 UserStorage、不复用 `[[agent_capture.ingest_tokens]]`。
- 不实现 pure SSH LFS、不改 trunk 分支准入（非 `main` 仍 B0 拒绝）、不重开 Cedar / CL。
- 不把 password 写入 tracing / `GIT_SSH_COMMAND` 样例（沿用 SP-02：`SSH_ASKPASS`）。

### 完成判据

- `push_auth=token` + `ssh_receive_pack=true`：真实 `git` 客户端用**同一密文**完成 HTTP receive-pack 与 SSH receive-pack，tip 进入 `refs/heads/main`，无 `refs/cl/*`。
- 同一部署上 `paths` 不覆盖的子路径 SSH/HTTP 均为 403 等价拒绝；未出示或错误密文不能写。
- `push_auth=none` + `ssh_receive_pack=true` 启动失败；`none` 默认栈 SSH push 仍报 `SSH receive-pack is disabled`。
- review 形态 SSH 公钥推送与「默认匿名开时不 `auth_none`」不被破坏。
- 部署文档写清：统一的是密文与写闸，不是「SSH 协议整面默认打开」。

### 审计证据、真实缺口与提升条件

- **Mega 证据**：N/A。
- **mega2 现状证据**：TP-20 / ADR-SP-01..04；`GitConfig::ssh_receive_pack_enabled`；`integration_git_ssh_trunk_token_push_receive_pack_disabled`；`deploy-trunk.md` §4。
- **最小可验证第一阶段**：日期计划先写 ADR（形态闸 + 谓词）并改 `ssh_receive_pack_enabled` / validate，配一条 SSH token push 进程 IT 与一条 none 启动拒绝；再补文档与 review 回归。
- **风险与边界**：password 进 SSH 有客户端泄漏面（ADR-SP-01 已接受于读路径）；打开写后泄漏面等于「能推」。必须保持常量时间查找、日志不回显密文。误把 `none` 与 `ssh_receive_pack=true` 组合放行会破坏 TP-20 安全边界——启动校验是硬门。

### 依赖与顺序

不依赖 PT-01..PT-12。消费已交付的 TP-19/20 与 SP-01/02。与 PT-03 streaming、PT-04 `DEFER-GM-02`（pure SSH LFS）无实现依赖。产品确认「CI/Agent 需要 SSH 推同一 token」后即可开日期计划；未确认前保持候选，不得在其它卡顺手打开 receive-pack。

## PT-14：trunk 历史只读投影视图

mega2 原生能力，Mega 证据为 `N/A`。P0 的设计契约见 [`../refactoring/history-projection.md`](../refactoring/history-projection.md)，实施与发布由 [`plan-20261002.md`](plan-20261002.md) 承接；本节记录长期范围与进入后续切片的条件。

### 现状缺口

P0 已交付过滤器代数（`src/ceres/view/filter/`）、ViewRepo（`src/ceres/pack/view_repo.rs`）、视图表（`src/jupiter/migration/m20261006_000100_add_view_tables.rs`）、追赶与 worker（`src/jupiter/service/view_projection_service.rs`、`src/jupiter/service/view_worker.rs`）及注册入口（`src/api/router/view_router.rs`）；`tests/integration_git_cli.rs`、`tests/integration_git_ssh.rs` 和 `tests/integration_views.rs` 覆盖真实客户端与故障应答。长期缺口见 PT-14 总览行所列 P1–P3 延后项；Docker 远端结果另由本计划 `DEFER-HP-20` 核对。

### 选定方案

从 `main@/` 的线性根链确定性计算只读派生对象，按规范化过滤器标识视图；只在派生表中维护状态，经 ViewRepo 输出 v0 / v2、HTTP / SSH Git 读协议。注册与重新预热受跨副本事务准入约束，异常停止追赶并返回可诊断错误。P0 只接受 trunk 与 sha1；设计语义和恢复程序以契约与日期计划为准。

### 目标范围

价值是让 Agent、镜像和真实 Git 客户端取得子路径物化前的完整历史，并可构建组合目录视图。P0 覆盖过滤器、根链与派生表、后台追赶、只读 URL、注册查询、配置及基准；持续轨道保留后续的在线回收、shallow、非线性历史和视图写入等需求。

### 非目标

视图不作为读权限边界，不接受 push，不替换现有 `/<path>.git` 的物化链。P1–P3 能力仍按日期计划中的延后登记逐项重新评审，不能因 P0 发布自动视为完成。

### 完成判据

相同根链与过滤器在清表重建和并发追赶后产生相同 tip；真实 Git 与 Libra 客户端能通过视图 URL clone 与 fetch；无效定义、未就绪、缺对象、根链不连续与写请求按契约被拒；注册配额、恢复步骤和基准满足日期计划验收。

### 审计证据、真实缺口与提升条件

- **P0 交付证据**：[2026-10-08 收口计划](plan-20261002.md)记录 36 张任务卡、34 个独立版本，最后版本 `v0.42.25` 的 tag 与非草稿 GitHub Release；HP-24 基准与 HP-25 Libra smoke 均在计划中留证。
- **真实缺口与提升条件**：P1 尚无承接日期计划；按总览行列明的 `DEFER-HP-*` 逐项立项。进入 P1 前复核真实使用、HP-24 基准、故障恢复与既有 trunk 路径回归。

### 依赖与顺序

依赖已交付的 trunk 根写入与协议分层；Libra smoke 消费 [`plan-20261001.md`](plan-20261001.md) 的 harness。P0 按日期计划的依赖图实施，P1–P3 在 P0 有运行证据后另立计划。

## PT-15：`v0.42.25` Docker 镜像按原提交精确补发

mega2 原生发布债务，Mega 证据为 `N/A`。**接收登记日期：2026-10-09**；来源为 [`plan-20260920.md`](plan-20260920.md) 的 `DEFER-OX-02`，接收 owner 角色为 **mega2 发布维护者**。登记接收只明确长期归属，不表示旧镜像已补发，也不解除源计划对 open 债务的审计。

### 现状缺口与边界

`v0.42.25` 有 Git tag 与非草稿 GitHub Release；2026-10-09 核对的 [Docker run `37704534619`](https://github.com/gitmono-dev/mega2/actions/runs/37704534619) 为失败，Docker Hub 尚无该版本镜像。该 run 的 `headSha` 为 `4b645af3035f8186f0ffdab3e39754667959afe0`；远端 `refs/tags/v0.42.25` 是注解标签对象，执行时必须重新解析 `v0.42.25^{}` 并与源提交和 Release 对照。[`plan-20260920.md`](plan-20260920.md) 的 FIX-OX-01 和后续 OX 卡验证**新 tag** 的 Docker D 门，不补旧版本，也不改变旧 run 的结论。PT-14 的 `DEFER-HP-20` 涵盖更多历史版本的远端结果，不能因本 PT 补一个镜像而整体关闭。

### 重启条件与最小切入点

- 发布维护者持有可核验的旧 tag、注解标签到提交的解析记录、Docker Hub 发布权限，以及只操作精确版本标签的独立补发流程；先确认目标标签不存在或已有 digest 可安全核对，遇到来源冲突即停止。
- 另立发布日期计划，pin `v0.42.25^{}` 的提交 SHA，从该提交树分别构建 `linux/amd64`、`linux/arm64`，记录构建 run、每架构 digest 与最终 manifest。补发流程不能由旧 tag 重跑时顺带更新 `latest`、minor channel 或其它可变标签，也不能重写 Git tag。

### 本计划之外的完成判据

两个版本标签 `genedna/mega2:v0.42.25` 与 `genedna/mega2:0.42.25` 解析到同一双架构 manifest，其 amd64/arm64 构建均可追溯到**同一个已核对的旧源提交**；`latest` 和其它可变标签在补发前后 digest 不变。新补发记录和 Release 注记分别链接旧失败 run、新构建/推送 run、源 SHA 与 manifest digest，并明确旧 run 仍为失败。只有这些远端证据齐备，接收方才可关闭 `DEFER-OX-02`；本 PT 不把补发计作任何 OX 卡的新 tag D 成功。

### 无人承接时的回落

若发布维护者未立项或发布权限不可用，`DEFER-OX-02` 保持 open；[`plan-20260920.md`](plan-20260920.md) 维护者在每次后续 release 审核中继续标注旧镜像缺口，PT-15 保持待排期。不得以新的 `latest` 镜像、一次本地 build 或旧失败 run 冒充补发完成。

## PT-16：外部 AI operator 实际接收与执行

mega2 原生外部集成，Mega 证据为 `N/A`。**接收登记日期：2026-10-09**；来源为 [`plan-20260920.md`](plan-20260920.md) 的 `DEFER-OX-03`，接收 owner 角色为 **mega2 AI 集成维护者**。登记接收只确定后续验证归属，真实目标尚未选定，也无外部执行证据。

### 现状缺口与边界

当前日期计划的 Q7 交付边界是具名 operator token、脱敏 `GET /api/v1/github-sync/bindings/{id}/status`、带 expected tips 的 `cursor-reset` / 独立 scope 的 `force` 命令，以及 OX-56/53 的 fake-client 本地回环。状态中的 remote tip 是附 `observed_at` 的最近观察值，不是实时 GitHub 保证；系统不会把 AI 文本、GitHub 侧 merge 或 `storage_events` best-effort 通知视作自动解 park。外部 AI 目标的选型、真实拉取、决策与命令执行不在 OX 卡的验收内。

### 重启条件与最小切入点

- 指定真实 AI 目标、运行 owner、专用具名身份与最小 operator scope，确认带外 GitHub 分歧处理的责任方及审计留存位置；缺任一项不开始真实命令验证。
- 在独立测试 binding 上先由目标拉取 Q7 status，理解 `parked` 原因、`observed_at` 与 CAS generation；经责任方在 GitHub 带外对齐后，目标再带 expected local/remote tips 调用安全的 `cursor-reset`。`force` 必须是另行授权的显式决定，不能由模型文本或状态轮询自动触发。

### 本计划之外的完成判据

后续日期计划记录真实目标的脱敏身份与读取时间、status 内容的关联 ID、分歧处置决定、带 expected tips 的一次成功命令及命令审计记录；同时证明旧 tips 返回 409、缺少命令或 force scope 返回 403，且没有凭据、原始 pack 或敏感 URL 泄露。目标运行日志与 mega2 状态转换可按 binding/generation 对照，确认结果来自真实目标而非 OX-53 fake client。证据齐备后接收方方可关闭 `DEFER-OX-03`；本地接口或回环通过本身只证明日期计划的边界。

### 无人承接时的回落

若无真实目标或具名维护者接手，`DEFER-OX-03` 保持 open，源计划维护者继续维持 Q7 接口和 fake-client 的已验证边界，禁止写“AI 已收到/已执行”。分歧仍按人工 operator 流程处理，绝不因外部集成空缺自动 force 或放宽鉴权。

## PT-17：Scoped Traces归档与Agent历史检索

**历史取消快照（2026-10-20 07:20:41 UTC）：** FIX-MTR-03/LTR-00 的后续验收分别按 DEFER-TR-FIX3-AC/DEFER-LTR-06 停止，两卡 blocked/空，引用方保持前驱阻塞；替代依赖须另行 ER-03/G-09 两仓双审。Mega2 原生 fetch 与只读远端 ref 均为 74ce530d；Libra 原生 HTTPS fetch/快进后 main=origin/main=6003a0c、只读 ref 同一，取消前 49 行 source pin 三行已漂移、旧静态脚本 exit1。H0 仍未 PASS；共享契约 H0 行仍具名延后的 LTR-00，替代生产者须经契约修订、重绑 hash 及 H0 双审。

**历史（2026-10-20，取消前快照）：** Libra e24c2d4；Mega2 本地 74ce530d、远端 SSH fetch 未核；C2 原件路线停止，R15/R16/R17 双 FAIL 均保留；R18 冻结输入治理修订 Claude/Codex 双 PASS（仅治理）；Mega2 远端未核实，H0 仍未 PASS；FIX3/FIX4/MTR-01/LTR00 blocked/空。

**历史（2026-10-20 03:44:17 UTC）：** Libra `fc56869`、Mega2 `74ce530d`；R13 双 FAIL 的 H0 映射旧状态 P1 已修，R14 双审待取得；无包 STOP，FIX3/FIX4/MTR-01/LTR-00 blocked/空。

**历史（2026-10-20 03:39:25 UTC）：** Libra `fc56869`、Mega2 `74ce530d`；R12 Claude FAIL/Codex PASS 的旧快照 P1 已修；用户确认无交接包，FIX3 同源诊断 STOP，旧 B8/3+8 保留；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-01 未开工；R13 当前 H0 双审待取得。

**历史（2026-10-20 00:38:10 UTC）：** Libra `main=ec77730a`、Mega2 `main=74ce530d`；R10 Claude FAIL/Codex PASS：两计划正文 DEP-TR-H0 行仍以旧 R6 为当前的 P1 已前滚，R11 终态由本字节的真实隔离双审输出裁定，不预填 PASS。FIX3 原 B8 FAIL/3+8 STOP 与 C2 原件缺失不变；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-00 AC-3/自身 ER-05 未过、formal 0/26，LTR-01 未开工；零新增诊断/A-B 窗或发布。 **历史（2026-10-20 00:34 UTC）：** 新 HEAD `ec77730` 的 49 路径/444 链接静态门通过；R9 Claude FAIL/Codex PASS 的索引与修订历史 P1 已前滚，R10 待双审。FIX3、FIX4、MTR-01 与 LTR-00 仍 blocked/空，零新诊断/A-B 窗；LTR-01 未开工。 **历史（2026-10-19 18:34 UTC）：** 新 FIX3 条件治理终态 R6 Claude/Codex 双 VERDICT: PASS（治理范围）；Claude 两项 P2 已具名修正：Libra R5 轮次加“新提案”限定、LTR00 B 静态摘要补 AC-4。旧失败/STOP、零新窗口及所有卡阻塞不变；本次纯文档修订的结论以隔离终态审查输出为准。 **历史（2026-10-19 18:30 UTC）：** 新 FIX3 条件恢复治理 R5 Claude/Codex 双 VERDICT: PASS（冻结输入，仅治理）；收据与 H0/LTR00 B 静态限定已前滚记账，终态文档待独立复核。原 C2 包、精确诊断合同、新窗口及交叉写者让出仍缺；FIX3/MTR-01/FIX4/LTR00 blocked/空，无卡验收、提交或发布授权。 **历史（2026-10-19 18:25 UTC）：** 新 FIX3 提案 R4 Claude/Codex 双 FAIL；LTR-00 现行 AC-1/AC-2/VER-2 曾写“待 R3”与已发生 R3 冲突，三处已改为不依赖轮次的“现行 H0 待双 PASS”；R5 待审。原 C2 包与精确诊断合同仍缺，零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空。 **历史（2026-10-19 18:21 UTC）：** 新 FIX3 提案 R3 Claude FAIL/Codex PASS；Libra 活跃总览 P1 已修，R4 待双审；FIX3/MTR-01/FIX4/LTR-00 blocked/空。**历史（2026-10-19 18:16 UTC）：** 新 FIX3 提案 R2 Claude/Codex 均 FAIL；原全量时限/连接压力及旧 R6 活跃索引两项 P1 已前滚修订，R3 待审；LTR-00/FIX3/MTR-01/FIX4 blocked/空，62张 pending。**历史（2026-10-19 17:39:15 UTC）：** H0 R6 Claude/Codex 双 PASS，仅旧输入的 LTR-00 审计资格。

**历史（2026-10-19 17:33:08 UTC）：** H0 R5 Claude FAIL/Codex PASS，日期索引旧下一卡 P1 已修，R6 待审。

**历史（2026-10-19 17:28:17 UTC）：** H0 R4 Claude FAIL/Codex PASS，主表旧下一卡 P1 已修，R5 待审。FIX3/MTR-01/FIX4 blocked/空，其他27张已发布依赖后继及MTR-10 blocked/complete，62张 pending。

**历史（2026-10-19 17:21:13 UTC）：** H0 R3 Claude FAIL/Codex PASS；四项 P1 已前滚修订，R4 待审。FIX3/MTR-01/FIX4 blocked/空，其他27张已发布依赖后继及MTR-10 blocked/complete，62张 pending。

**历史（2026-10-19 17:12:27 UTC）：** H0 R2 Claude/Codex 双 FAIL 已保留；旧 Phase0/M0、MTR-01 H0 勾选、FIX3 越界写集与索引漂移修订后待 R3。FIX3 与 MTR-01 blocked/空，其他27张已发布依赖后继及 MTR-10 blocked/complete；MTR-10 Docker 三 job success，MTR-38/LTR-01 不开工。

**历史（2026-10-19 16:59:27 UTC）：** H0首审 Claude/Codex 均 FAIL，FIX3 原 AC-1/VER-5 未证，当前 `blocked/空`；28张曾取得自身/继承C/D的依赖后继当前 `blocked/complete`。MTR-10 已发布 `v0.42.56`，C①–⑧真实完成，Docker run `39288093340` 仍执行，同时因前驱保持 `blocked/remote-pending`；MTR-38/LTR-01 不启动。旧B8 FAIL、3/3与8/8 STOP保持，已发布版本不撤回。

**历史（2026-10-19 16:45:48 UTC）：** MTR-10 当时记为 `in-progress/remote-pending`；R2/R3 独立 Claude/Codex 双 PASS，最终版本树三门、全量 2957 passed/0 failed/3 既有 ignored、双 build 均通过。C⑤–⑧已由提交 `74ce530d`、远端 main/tag `v0.42.56` 与手写 Release 证明；tag Docker run `39288093340` 尚在执行。MTR-38、Libra LTR-01 仍不可越过前驱。

**历史（2026-10-19 16:38:20 UTC）：** MTR-10 当时本地 C①–④通过，C⑤–⑧与 D 未取得。Libra 远端 main 已由并发写者前进至 `cbee959c`，本地编辑前须先同步。

**历史（2026-10-19 11:55:43 UTC）：** MTR-87 为 done/complete。发布提交 `2b3013f`，tag `v0.42.55`，Docker run `39246051261` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39245968977` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-10。LTR-01 未开工。不推送 Libra。

**历史（2026-10-19 11:35:33 UTC）：** MTR-87 当时为 in-progress/locally-accepted。版本面 `0.42.55`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-19 07:19:18 UTC）：** MTR-36 为 done/complete。发布提交 `db0d198`，tag `v0.42.54`，Docker run `39213387737` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39213294512` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-87。LTR-01 未开工。不推送 Libra。

**历史（2026-10-19 06:58:32 UTC）：** MTR-36 当时为 in-progress/locally-accepted。版本面 `0.42.54`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-19 03:51:58 UTC）：** MTR-09 为 done/complete。发布提交 `253812f`，tag `v0.42.53`，Docker run `39195273240` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39195249238` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-36。LTR-01 未开工。不推送 Libra。

**历史（2026-10-19 03:31:42 UTC）：** MTR-09 当时为 in-progress/locally-accepted。版本面 `0.42.53`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-19 02:00:24 UTC）：** MTR-37 为 done/complete。发布提交 `7b2ec86`，tag `v0.42.52`，Docker run `39186530527` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39186498218` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-09。LTR-01 未开工。不推送 Libra。

**历史（2026-10-19 01:39:00 UTC）：** MTR-37 当时为 in-progress/locally-accepted。版本面 `0.42.52`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-18 23:37:15 UTC）：** MTR-08 为 done/complete。发布提交 `c585280`，tag `v0.42.51`，Docker run `39176083436` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39176048014` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-37。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 23:17:00 UTC）：** MTR-08 当时为 in-progress/locally-accepted。版本面 `0.42.51`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-18 22:25:20 UTC）：** MTR-07 为 done/complete。发布提交 `4e51fc7`，tag `v0.42.50`，Docker run `39171166787` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39171131738` 收口时仍为 in_progress，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-08。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 22:09:00 UTC）：** MTR-07 当时为 in-progress/locally-accepted。版本面 `0.42.50`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-18 20:23:25 UTC）：** MTR-47 为 done/complete。发布提交 `eca4895`，tag `v0.42.49`，Docker run `39161773361` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39161741248` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-07。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 20:04:17 UTC）：** MTR-47 当时为 in-progress/locally-accepted。版本面 `0.42.49`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-18 19:12:05 UTC）：** MTR-48 为 done/complete。发布提交 `e00d36b`，tag `v0.42.48`，Docker run `39156256515` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39156207886` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-47。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 18:50:56 UTC）：** MTR-48 当时为 in-progress/locally-accepted。版本面 `0.42.48`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-18 17:50:33 UTC）：** MTR-06 为 done/complete。发布提交 `b3dedc7`，tag `v0.42.47`，Docker run `39150079387` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。Harbor run `39150028422` conclusion 为 success，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-48。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 17:26:00 UTC）：** MTR-06 当时为 in-progress/locally-accepted。版本面 `0.42.47`。全量测试 exit 0。当时推送、tag、Release 与 Docker D 尚未取得。

**历史（2026-10-18 10:03:27 UTC）：** MTR-45 为 done/complete。发布提交 `e991daa`，tag `v0.42.46`，Docker run `39116566641` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-06。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 09:42:59 UTC）：** MTR-45 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 07:51:29 UTC）：** MTR-05 为 done/complete。发布提交 `d31e962`，tag `v0.42.45`，Docker run `39108142891` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-45。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 07:26:12 UTC）：** MTR-05 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 06:24:48 UTC）：** MTR-65 为 done/complete。发布提交 `821f748`，tag `v0.42.44`，Docker run `39103156765` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。当时下一张卡是 MTR-05。LTR-01 未开工。不推送 Libra。

**历史（2026-10-18 06:09:24 UTC）：** MTR-65 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 05:19:33 UTC）：** MTR-83 为 done/complete。发布提交 `ea0d309`，tag `v0.42.43`，Docker run `39099140187` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-18 04:59:53 UTC）：** MTR-83 当时为 in-progress/locally-accepted。第二次全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 04:06:43 UTC）：** MTR-82 为 done/complete。发布提交 `8b745c7`，tag `v0.42.42`，Docker run `39094965900` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-18 03:47:39 UTC）：** MTR-82 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 03:08:59 UTC）：** MTR-81 为 done/complete。发布提交 `27d577a`，tag `v0.42.41`，Docker run `39091727196` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-18 02:52:25 UTC）：** MTR-81 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 02:11:19 UTC）：** MTR-80 为 done/complete。发布提交 `51419a8`，tag `v0.42.40`，Docker run `39088366533` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-18 01:56:14 UTC）：** MTR-80 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 01:17:49 UTC）：** MTR-79 为 done/complete。发布提交 `65dbb37`，tag `v0.42.39`，Docker run `39084831115` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-18 00:57:34 UTC）：** MTR-79 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-18 00:18:09 UTC）：** MTR-78 为 done/complete。发布提交 `2415fb8`，tag `v0.42.38`，Docker run `39081203072` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-17 23:58:19 UTC）：** MTR-78 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-17 22:58:26 UTC）：** MTR-77 为 done/complete。发布提交 `707ddd8`，tag `v0.42.37`，Docker run `39076153505` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-17 22:40:18 UTC）：** MTR-77 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-17 21:53:03 UTC）：** MTR-76 为 done/complete。发布提交 `87eed19`，tag `v0.42.36`，Docker run `39071597649` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-17 21:33:14 UTC）：** MTR-76 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-17 20:43:41 UTC）：** MTR-50 为 done/complete。发布提交 `e8bc7cb`，tag `v0.42.35`，Docker run `39066690076` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-17 20:22:35 UTC）：** MTR-50 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-17 19:11:57 UTC）：** MTR-04 为 done/complete。发布提交 `ce1fe9d`，tag `v0.42.34`，Docker run `39060072328` 的 amd64、arm64 与 job `docker` 均为 success。本收口不再次 bump。

**历史（2026-10-17 17:59:09 UTC）：** MTR-04 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-17 17:02:25 UTC）：** MTR-72 为 done/complete。发布提交 `d6e3cb0`，tag `v0.42.33`，Docker run `39051067466` success。

**历史（2026-10-17 16:45:27 UTC）：** MTR-72 当时为 in-progress/locally-accepted。全量测试 exit 0。Docker D 当时尚未取得。

**历史（2026-10-17 16:10:00 UTC）：** MTR-72 为 in-progress/locally-accepted。VER-1..8 exit 0。当时全量与 Docker D 尚未取得。

**历史（2026-10-17 15:42:03 UTC）：** MTR-70 为 done/complete。发布提交 `35d5457`，tag `v0.42.32`，Docker run `39045161292` 的 amd64、arm64 与 job `docker` 均为 success。Harbor run `39045116559` conclusion 为 cancelled，不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。

**历史（2026-10-17 14:19:44 UTC）：** MTR-03 为 done/complete。发布提交 `959c1e2`，tag `v0.42.31`，Docker run `39039264618` 的 amd64、arm64 与 job `docker` 均为 success。Harbor run `39039211625` 不代替该 tag Docker。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。

**历史（2026-10-17 14:01:13 UTC）：** MTR-03 当时为 in-progress/locally-accepted。版本面 `0.42.31`。全量测试 exit 0。Claude 与 Codex 均为 PASS。推送、tag、Release 与 Docker D 当时尚未取得。

**历史（2026-10-17 MTR-02 收口）：** FIX-MTR-03 已以 v0.42.28 自行发布，FIX-MTR-01 已以 v0.42.29 自行发布（均 done/complete）。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。MTR-02 为 done/complete（`b537ced`，tag `v0.42.30`，Docker run `39034027014` success）。MTR-01 继承该 C/D，为 done/complete。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-03。LTR-01 未开工。不推送 Libra。

**历史（2026-10-16）：** G-13 已退出并回到 G-07。FIX-MTR-03 仍 blocked，恢复窗已执行，AC-1 未勾选。FIX-MTR-01 在其后自行发布。FIX-MTR-04 改为 docs/no-release，不再 bump。formal 0/95。

**历史迁机WIP（原B8失败保留）：** 迁机WIP检查点（2026-10-15 17:20:26 UTC）：按用户最新“提交mega2和libra所有内容、换机继续开发”要求，拟保存到 `handoff/scoped-traces-20261016`；这是明确的未发布工作保存，root仅将最新指令解释为原G13不推送窗口的传输范围变更，不是模板EX或正式任务发布。FIX3测试代码GREEN SHA `2b7cdd4ab78dd335dcd711be7d454aa30c11db34609a9c86ca43602b5eb60e4b`：受控旧10秒目标RED101、新180秒GREEN1/1、原exact1/1、默认module8/8、fmt0、strictClippy0；生产prefix未改、临时observer已撤回。C2具体DB机制未知，第四oracle归属仍是source/control-flow/runtime推断与root本地判断，own实施ER05/G13最终准入未取得。B8于17:13:38..17:13:42 UTC实际exit1，仅因plan-status引用空行尾空格触发native diff --check exit2；B报告SHA `57a4b782717c2a1295a7471094fb46b963ba06514a6ff1b102b9dbdbb45b4e20`、日志SHA `56c77d16bea5e9e872d657aa8ac09e6488f827ed21bc024f31ecd230c441401d`。3诊断/8AB名额已耗尽且STOP保留，不重跑、不重置、不晋升；FIX3现blocked/空，FIX4、MTR01及LTR01继续blocked，formal0/95与0/26。尾空格只作交接卫生修正，后续diff检查或交接审查不能补记原B8成功。版本保持0.42.27，不改main远端、不创建版本tag/Release。

**历史接机指令，不再执行：** 另一台机器先获取两仓此同名分支并核对提交OID、原始source/测试摘要与签名；Libra本地e98源与上游存在分叉，先检查最新origin/main并保留上游全部COV/OG工作后精确整合，不以旧552/33a5 CI证明新树。重新做ER01/ER02/ER03、共享写集交接及本机服务/工具链/凭据核验，原T3/控制slot/STOP不能续用或删除，不能把换机当预算重置。先按实际失败重新规划文档B及自身Claude/Codex实施ER05/G13判断，必要的实际验收重新绑定新树；再按FIX5→FIX2→FIX1→FIX3→FIX4→MTR01及跨仓H门推进。FIX4仍须自己的A/B/ER05、parity和真实patch bump、完整C五门、真实tag Docker D；默认Darwin opt0告警仍未修，opt1仅原已审本机验收环境。用户只要求本次保存交接，本机不续后续功能卡。

**以下本地A/B8输入记录为17:12历史快照，不代表B8完成：**

**17:12历史快照A（B8输入原文保留）：** **当前FIX3本地A事实、B8待实际结果：** FIX3本地A/AC记账（2026-10-15T17:12:44.326695+00:00）：真实前7条formatcopy0/目标RED101/同formatted180GREEN0/原exact0/默认module0/fmt0/strict0已由root消费，RED101仅目标观察、非验收PASS；具名AC1/VER5判断SHA 4c7add7486c236fcbade4e6528e22d0bf8c1c6e08e94aa7bed558b902ce5890d支持原C2严格阻断归属第四oracle结构代码缺陷，C2具体DB机制未知、第四身份仍属可复核控制流/runtime推断。全部4AC/5VER仅记本地实际A事实；B8输入仍in-progress/空、B实际0/ownER05/G13终判及FIX4最终C/D均pending，本卡未提交推送或版本发布。原full INCOMPLETE/legacy STOP缺失与stale2/derived3不改；T3原18:26截止/3diag耗尽/8AB总额不重置。root事实SHA 1721926d88164ae1aedbf54ebcc5063dc3c8ff03adf51f24f065d78e22ff4f81。 实际R5条件双审root收据SHA 81ee5d00246c78f4ee0cda2c78e0feefa652545f6d389936bef08b430593c922、唯一控制面实际0收据SHA 9ddd2268aaeadb52460cf7b3e4ecfa787631906cfdf0845d118baa4a9ba561bf、root源码/AB GO收据SHA ea7d6e9dbc44fde944a110bd24bd716c2d5bf957aaf2153628aefc309b288b48只授本轮有界验证，不是本卡ownReview/G13最终准入。

**17:12历史快照B（重复输入说明原文保留）：** Mega2原生能力，承接用户2026-10-14/15需求。[Mega2日期计划](plan-20261015.md)与[Libra对偶计划](../../../libra/docs/development/plan/plan-20261015.md)按H0..H6推进，唯一协议源为[`traces.md`](../refactoring/traces.md)。FIX3本地A/AC记账（2026-10-15T17:12:44.326695+00:00）：真实前7条formatcopy0/目标RED101/同formatted180GREEN0/原exact0/默认module0/fmt0/strict0已由root消费，RED101仅目标观察、非验收PASS；具名AC1/VER5判断SHA 4c7add7486c236fcbade4e6528e22d0bf8c1c6e08e94aa7bed558b902ce5890d支持原C2严格阻断归属第四oracle结构代码缺陷，C2具体DB机制未知、第四身份仍属可复核控制流/runtime推断。全部4AC/5VER仅记本地实际A事实；B8输入仍in-progress/空、B实际0/ownER05/G13终判及FIX4最终C/D均pending，本卡未提交推送或版本发布。原full INCOMPLETE/legacy STOP缺失与stale2/derived3不改；T3原18:26截止/3diag耗尽/8AB总额不重置。root事实SHA 1721926d88164ae1aedbf54ebcc5063dc3c8ff03adf51f24f065d78e22ff4f81。 实际R5条件双审root收据SHA 81ee5d00246c78f4ee0cda2c78e0feefa652545f6d389936bef08b430593c922、唯一控制面实际0收据SHA 9ddd2268aaeadb52460cf7b3e4ecfa787631906cfdf0845d118baa4a9ba561bf、root源码/AB GO收据SHA ea7d6e9dbc44fde944a110bd24bd716c2d5bf957aaf2153628aefc309b288b48只授本轮有界验证，不是本卡ownReview/G13最终准入。

旧[`agent-capture.md`](../refactoring/agent-capture.md)仍描述现行实现；只有H6实际发布和清除证据齐备后才标退场。MegaUI、向量/自动总结、event/file-op模型及未知来源的旧Git traces自动迁移均按日期计划DEFER-MTR登记，不扩大PT-16外部operator范围。收口须核对跨仓无环可用性、raw bytes/权限/索引预算/clone隔离、旧12表与owned历史对象完整清除、各卡C/D及终态证据。

---

## 实施顺序

十一个移植项按四个阶段推进。阶段之间是架构依赖，不要求前一阶段全部结束才开始下一阶段的设计，但不得绕过前置决策直接实施高风险切片。

### 当前执行任务：[`plan-20260820.md`](plan-20260820.md)（Vault vendored → `libvault` 0.3.0）

`plan-20260803.md` / `plan-20260812.md` 已完成。`plan-20260820.md`（方案 B：`libvault = "0.3.0"` 替换 `src/vault/` vendored；UN-31 只读语义迁到集成层）已于 2026-08-21 完成。PT-01 全部非延后卡已完成；PT-04 剩余 `DEFER-GM-01..05` 为 follow-up；`plan-20260812` 授权闭环已收口。长期仍按优先级推进 PT-02 逐提交归类；orion 三件套仍是最大整体缺口，但实施前须先完成 PT-05 的通信形态决策。

### 阶段零：工程安全基线

1. SB-01 收敛 protocol/config 记录的残余 panic 清单，并把"移植不引入新 panic"门禁化。
2. SB-02 延续 redaction / SecretRef 覆盖扩展（本仓 `mail.password` 已随 ADR-WA-08 移除）。
3. SB-03 配合 PT-01 落地测试栈拓扑扩展规范。

### 阶段一：基建、同步与协议基线

1. PT-01 完成测试基建扩展与 Git CLI 最小矩阵。
2. PT-04 按 `plan-20260803.md` 交付真实 Git CLI 完整矩阵并进入 CI。
3. PT-02 完成 `#2130..#2169` 基线追平与持续同步机制。
4. PT-03 在 PT-04 矩阵上完成 streaming 与 repo/path ACL 收敛。

阶段完成后，测试基建可承载全部后续 PT，mega2 与 Mega 的已移植模块重新对齐，协议层有防回归网。

### 阶段二：事件面与构建链路

1. PT-05 完成 bus/infra 审计与归位决策。
2. PT-06 交付 orion-server 最小环并与 bellatrix 联通。
3. PT-07 交付 orion agent（stub buck2 先行）与端到端链路。

阶段完成后，构建链路从 dispatch 到执行全通。

### 阶段三：弹性调度与客户端补齐

1. PT-08 完成客户端 API 面补齐与 scheduler 移植/替代决策。

### 阶段四：收尾类（可与阶段一至三并行）

1. PT-09 build 完成触发器、次渠道 retry 与多实例矩阵（出站只剩 generic webhook；对象改为 webhook）。
2. PT-10 热加载消费者扩展、生命周期审计与持续 diagnostics（对象存储/Redis SecretRef、`[oauth]` 已落地）。
3. PT-11 格式版本策略、KEK 轮换、异地审计 sink、root recovery。

### 持续轨道：前端与账户系统一致性

PT-12 不独占阶段，贯穿各阶段推进：会话信任路径与 compose 同栈 IT 已由 plan-20260731 交付；下一动作为全量双前端功能对照审计（纯审计，可与阶段一并行），随后按功能域把追平切片排入日期计划；每个涉及公开 API/账户行为的 PT 在实施时必须通过原则 11 的前端影响评估。

### 持续轨道：storage-only 统一推送密文

PT-13 不进入 Mega 移植阶段。前置事实（HTTP token 写、SSH token 读、SSH 写关闭）已由 plan-20260905 / plan-20260908 交付。下一动作是产品确认后开日期计划，按「选定方案」把 SSH receive-pack 接到同一 `lookup_push_token` 写闸；未确认前不得改 `ssh_receive_pack_enabled`。

### 持续轨道：trunk 历史只读投影视图

PT-14 的 P0 已按 [`plan-20261002.md`](plan-20261002.md) 收口。下一动作是依据真实使用与 HP-24 基准结果为 P1 立项日期计划，再逐项审定 P2–P3 延后能力。

### 持续轨道：旧版本 Docker 补发与外部 AI operator

PT-15 仅在精确旧源提交、版本标签隔离流程和发布权限齐备后另立补发计划；PT-16 仅在真实 AI 目标、具名身份与带外处理 owner 落定后另立集成验证计划。两项接收登记均不减少 [`plan-20260920.md`](plan-20260920.md) 对**新版本 Docker D 门**及 Q7 本地接口/fake-client 的完成义务。

## 依赖图

```mermaid
flowchart TD
    PT01[PT-01 集成测试基建]
    PT02[PT-02 Mega 基线追平]
    PT03[PT-03 协议兼容收尾]
    PT04[PT-04 协议测试矩阵]
    PT05[PT-05 ceres bus/infra 归位]
    PT06[PT-06 Orion Server]
    PT07[PT-07 Orion Agent]
    PT08[PT-08 Scheduler 与客户端]
    PT09[PT-09 通知邮件收尾]
    PT10[PT-10 配置 SecretRef 收尾]
    PT11[PT-11 Vault 安全收尾]
    PT12[PT-12 前端与账户一致性]
    PT13[PT-13 storage-only 统一推送密文]
    PT14[PT-14 trunk 历史只读投影]
    PT15[PT-15 v0.42.25 Docker 精确补发]
    PT16[PT-16 外部 AI operator 实际执行]

    PT01 --> PT04
    PT01 --> PT06
    PT01 --> PT09
    PT02 --> PT03
    PT02 --> PT05
    PT02 --> PT06
    PT02 --> PT12
    PT04 --> PT03
    PT05 --> PT06
    PT06 --> PT07
    PT07 --> PT08
    PT05 --> PT09
    PT06 --> PT09
    PT06 --> PT12
    PT11 --> PT09
    PT13
    PT14
    PT15
    PT16
```

## 跨功能验收门禁

任何 PT 进入实施时，除具体功能验收外，还必须满足以下共同门禁：

### 数据正确性

- 所有 DB schema 变更走 `src/jupiter/migration/` 真实迁移，禁止手写 schema SQL；实体/存储/迁移三处同步（`AGENTS.md` 流程）。
- 与 Mega 共享的 schema 保持对齐，差异必须有书面豁免。
- crash window、并发 claim、重复请求与 idempotency 有故障注入测试。

### 安全与隐私

- 协议与网络输入一律 fallible parser；客户端输入不得导致进程崩溃。
- 凭据一律 SecretRef/SecretString；错误、日志、配置回显执行 redaction 并有活进程脱敏门禁。
- Vault 相关操作 fail-closed；key 丢失不自动重建，必须配恢复手册。
- 认证上下文（SSH publickey → commit binding）不得匿名降级。

### 机器接口

- HTTP API 变更同步 OpenAPI（utoipa）与 `src/contract/api/` DTO；新错误码同步 `docs/errors.md`。
- CLI 子命令按 `AGENTS.md` 流程注册（`builtin()`/`builtin_exec()`）并补解析测试。
- 列表类接口有分页、limit 和资源上限。

### 兼容与迁移

- 与 Mega 的 wire 行为、API 契约、DB schema 默认保持一致；偏离必须有 ADR。
- capability advertise 诚实：未实现且未测试的能力不声明。
- 配置变更遵守集中校验与热加载白名单纪律；破坏性配置变更需兼容窗口。

### 性能

- 协议热路径（pack 传输、LFS）不因移植退化为全量缓冲；streaming 改造带基准。
- 对象存储、大附件、构建日志采用 streaming/chunking，避免无界内存加载。
- 通知 outbox 的批次/并发限流默认值不因新渠道退化。

## 不进入本长期移植计划的 Mega 资产

以下 Mega 资产经审计不进入 PT-01 至 PT-12，需要时另行单独决策：

- **`moon/` Web 前端**（pnpm + turbo + Next.js）：非 Rust 资产，代码不移植进 mega2 仓库；mega2 的前端与账户系统由 monoui `mega2` 分支 `apps/next-app` 承载，两个前端体系的功能一致性义务由 PT-12 治理。
- **`scripts/`**（crates-sync、init_mega、demo、import-buck2-deps、webhook_receiver.py）：运维/开发辅助脚本，按 mega2 实际需要逐个评估，不做整体移植。
- **`docker/`**（demo、deployment）：部署形态与 mega2 的 docker-compose 测试栈是不同层次；部署资产按运维需求单独决策。
- **`BUCK`、`ci/`、`LICENSE-*`**：构建定义与 CI 形态已按 mega2 自身体系（Cargo workspace、`.github/workflows/`）建立，不回移植。
- **本仓 Campsite 风格 chat / Notes**：源系统是 campsite Rails 而非 Mega；产品面已由 [`plan-20260731.md`](plan-20260731.md) 整栈退场（能力留在 website），不占 PT 编号、不再作为本仓在维护基础能力。

## 日期计划索引

日期计划只承接明确切片；它完成后仍须回到本表按 PT 完成判据复核。

| 日期计划 | 对应 PT | 当前状态 | 范围与长期剩余缺口 |
|---|---|---|---|
| [`plan-20260727.md`](plan-20260727.md) | PT-01 | **已完成**（D 组绿于 v0.1.177） | 最终卡集 IT-01、IT-02、IT-03、IT-04、IT-06、IT-07、IT-08、IT-09、IT-10、IT-11、IT-12（共 11 张任务卡）；IT-05 已关闭为非任务卡（热加载黑盒此前已落地）。交付：测试双层规范、git-cli runner 拓扑、HTTP clone/push 最小矩阵、认证边界与失败路径、CI 执行/触发面、git/git-lfs 客户端 pin、并发 dispatcher 基线、P2 清单收口。不覆盖 SSH/LFS 矩阵（PT-04）、真多进程黑盒（PT-09） |
| [`plan-20260731.md`](plan-20260731.md) | PT-09 / PT-10 / PT-12（切片） | 已完成 | Website Better Auth 会话接入、`[oauth]` 落地、chat/notes 整栈退场、`website-next` compose 同栈 IT、本仓邮件投递迁出（ADR-WA-08；MN 含 compose/env/CI/文档同步）。不覆盖 PT-12 全量 moon↔next-app 功能对照（仍候选）；Slack/webhook 已由后续版本交付，仍不覆盖 build 触发器/retry/多实例收尾 |
| [`plan-20260802.md`](plan-20260802.md) | PT-09（切片） | 已完成 | Website 内部产品邮件 API 与 mega2 client 契约，关闭 DEP-06；不覆盖本仓非邮件通知的多实例语义或 PT-12 全量对照 |
| [`plan-20260803.md`](plan-20260803.md) | PT-04 | 已完成 | Git 使用场景测试补全：HTTP `pull`、匿名读关闭、HTTP LFS、cargo-native SSH、Mega 夹具 go/no-go（NO-GO 转 `DEFER-GM-03`）及 CI 归属均已交付（GM-12 发布于 v0.2.11，完成度复审收口于 v0.2.15）；不改 Git protocol 产品实现，补测发现的缺陷转 PT-03。不覆盖 ImportRepo 全命令矩阵、pure SSH LFS、Mega 大夹具移植、shell SSH 广度 CI、missing-repo 探针（`DEFER-GM-01..05`，PT-04 follow-up） |
| [`plan-20260812.md`](plan-20260812.md) | PT-03 / PT-12（切片） | 已完成 | 用户体系统一：website 认证 × mega2 授权；Cedar 三态、push 门、merge 面鉴权、只读审计（UN-31/UN-43）等；收口 v0.2.66。不覆盖身份键全量迁移（handoff PT-12） |
| [`plan-20260820.md`](plan-20260820.md) | PT-11（依赖形态切片） | **已完成（2026-08-21）** | vendored → `libvault` 0.3.0 已落地；VLT-S1 判定 **go**（shadow-unseal），VLT-S2 与 DEFER-VLT-01 未触发；`REL-VLT-RO`（VLT-02→04→FIX-VLT-01→**VLT-05 release**）；UN-31 八 AC 在集成层等价重建，十二条 `un31_` 与四条 `un43_` 全绿；FIX-04/un31 已迁至 `src/contract/vault/integration/` |
| [`plan-20260903.md`](plan-20260903.md) | PT-03（准备切片） | **已完成** | Monorepo 空仓初始化按 `monorepo.object_format` 生成 SHA-1/SHA-256 object graph；当时 BLAKE3 fail-closed。DEFER-HSH-01/02 的本仓可交付部分由 [`plan-20260907.md`](plan-20260907.md) 关闭；DEFER-HSH-03（原地 conversion / 权威 format metadata）仍延后。 |
| [`plan-20260907.md`](plan-20260907.md) | PT-03（hash 切片） | **已完成**（B3-06 / v0.8.67） | 钉 git-internal 0.9.0；显式 `HashKind`；blake3 bootstrap + Libra/git-internal wire/pack normal service；Git/LFS 独立 hash-domain（`Git=blake3/LFS=sha256` 与 `Git=sha256/LFS=blake3` 可表达）。不宣称标准 Git BLAKE3 互通（DEFER-B3-01）；不做原地 migration（DEFER-HSH-03）；LFS BLAKE3 业务面 `DEFER-B3-LFS-01`。 |
| [`plan-20260901.md`](plan-20260901.md) | PT-03（FastCDC Media 切片）+ N/A（receive-pack/导入/DB/Redis/ID；不新增 PT/SB） | **已完成**（FC-14 `0.9.0`；独立卡至 **v0.10.0** / `634904b`） | `--features fastcdc` Media：`libra/media/v1`、scope、finalize+LFS fallback、Libra FC-15 interop；receive-pack report-status/CAS delete、filepath CASE batch、DB deadlock retry、RedLock 10ms、Snowflake env/Redis/hash（保留 6+8）。不覆盖压缩/range-read、Campsite identity、crates-sync、Media GC、monoui 客户端、共享 fixture crate（`DEFER-FC-01..06`）。 |
| [`plan-20260904.md`](plan-20260904.md) | N/A（storage-only trunk 产品 API 写文件；非 Mega PT） | **已完成**（AW-05 / v0.8.61） | AW-01..05：push_auth 门控 + MonoWriteQueue tip 落地；API 写后 git clone/pull 可见；compose 可继承黑盒脚本。不重开 CL/Cedar；不打开 SSH receive-pack。 |
| [`plan-20260905.md`](plan-20260905.md) | N/A（trunk / MonoWriteQueue；非 Mega PT） | **已完成** | Trunk 直推与 storage-only；LFS 当时关闭（GAP-13/TP-18），由 [`plan-20260909.md`](plan-20260909.md) supersede。 |
| [`plan-20260909.md`](plan-20260909.md) | N/A（storage-only LFS；承接 TP-18 关闭后的独立议题） | **已完成** | LFS 批/锁鉴权对齐 `push_auth=none`/`token`；trunk 重新挂载；IT + 文档 supersede。不覆盖 SSH LFS transfer、PUT 逐请求鉴权、DEFER-TP-05。 |
| [`plan-20260906.md`](plan-20260906.md) | N/A（storage-only / trunk compose Git 黑盒；非 Mega PT） | **已完成** | 一 case 一卡；compose 黑盒；客户端=`git`/`git-lfs`/`ssh`（禁止 libra 作协议客户端）；SO-01 harness → HTTP 读/写/none/LFS → SSH 读 + reject receive-pack → SO-06 收口。 |
| [`plan-20260908.md`](plan-20260908.md) | N/A（storage-only SSH 只读；非 Mega PT）。SSH 写统一密文见 **PT-13** | **已完成** | **SP-01..SP-05**：SSH upload-pack 对齐 HTTP 读；`auth_none`/none + password-token；SP-04 cancelled→SP-01；`DEFER-SP-02`（SSH receive-pack + 同一 push token）由 PT-13 承接，本计划未打开 SSH 写。 |
| [`plan-20260910.md`](plan-20260910.md) | N/A（merge_writer sunset；非 Mega PT） | **已完成** | CL merge 唯一写者 = MonoWriteQueue；删除 `[monorepo].merge_writer`、Legacy processor、`merge_queue` 表。不删除 `/merge-queue` HTTP 门面（DEFER-MW-01）。 |
| [`plan-20260902.md`](plan-20260902.md) | N/A（storage-only OCI Distribution；非 Mega PT） | **已完成** | DR-01..DR-15：`[oci]` 双重门、4 表元数据、`ObjectNamespace::Oci`、`/v2` 最小 push/pull + tags/list、Basic 复用 push token、两遍流式 complete；文档见 [`../refactoring/oci.md`](../refactoring/oci.md)。不覆盖 `_catalog`/referrers/删除 GC/token 服务（DEFER-DR-01..06）。DR-15 收口 v0.8.53；完成判据文档收口对照代码勾选。 |
| [`plan-20260911.md`](plan-20260911.md) | N/A（storage-only Agent Capture；非 Mega PT） | **已完成** | libra 两套 session 以 raw 落入 `agent_capture_*` + `ObjectNamespace::Agent`；独立 ingest token；review 不挂载。不实现 libra 客户端（DEFER-AC-01）、不默认 FileHistory 50 版（DEFER-AC-02）。 |
| [`plan-20260912.md`](plan-20260912.md) | N/A（storage-only 提交后出站事件；非 Mega PT） | **已完成**（WH-01..WH-15 全部交付；WH-14 → v0.10.39，WH-15 → v0.10.40） | `[storage_events]` 默认关闭、storage-only token/none 可启用、restart-required。HTTPS HMAC 运输 + DNS 地址钉住 + 有界 emitter + 启动 secret 绑定 + CLI/service 清理尾段均已落地；六类来源 hook 全部挂上（`repo.push` / `oci.manifest.published` / `lfs.object.uploaded` / `lfs.media.finalized` / `agent_capture.events.committed` / `agent_capture.checkpoint.committed`）。2026-09-16 完成度复核追加的 WH-14（配置范围/规范名门）与 WH-15（`installation_id` 记录字段与 drop 记账）已于 2026-09-17 分别以 v0.10.39 / v0.10.40 发布。无 webhook CRUD/outbox/retry；presigned 直传与 media 静态 token 认证为 `DEFER-WH-01`/`DEFER-WH-02`。 |
| [`plan-20260917.md`](plan-20260917.md) | N/A（Libra `mega2 browser` 所需产品 HTTP 补全；非 Mega PT，不新增 PT 编号） | **已完成**（LB-01..LB-07 全部 done/complete；LB-02 → v0.10.41、LB-03 → v0.10.42、LB-04 → v0.10.43、LB-05 → v0.10.44，每次 bump 均有同名 tag 与手写 GitHub Release） | `POST /api/v1/delete-entry`、`POST /api/v1/move-entry`（目录变更走既有 `commit_tree_update` / `land_api_tip_push`，trunk 无 CL）；`tag_router` 挂到 storage-only 并对 create / delete 走 `push_auth`（create 注解 201 → 200）；compose 黑盒新增 delete / move / tags / unauth 四个 case；契约页 `docs/refactoring/directory-entry-api.md` 含可 pin 的「Libra pin」节（八条表面，钉在 v0.10.44）。不覆盖：get/delete 的 path 选择器与 path 隔离（DEFER-LB-03 / DEFER-LB-11；**隔离面已由 [`plan-20260918.md`](plan-20260918.md) 关闭**，目标归属仍延后）、`search_tree_for_update` 类型化 400（DEFER-LB-07）、README 产品写列表（DEFER-LB-08，用户维护）、`find_git_repo_like_path` SQL 绑定与 `/` 边界（DEFER-LB-09）、读 tree 与落地的窗口（DEFER-LB-10）、Libra 客户端本身（DEP-LB-03，由 Libra 计划承接） |
| [`plan-20260918.md`](plan-20260918.md) | N/A（文件删移与 Tag 契约跟进；非 Mega PT，不新增 PT 编号） | **已完成**（FT-01..FT-09 全部 done/complete；FT-02 → v0.10.45、FT-03 → v0.10.46、FT-04 → v0.11.0、FT-05 → v0.11.1、FT-06 → v0.11.2、FT-07 → v0.11.3；FT-04 起每次 bump 均有同名 tag 与手写 GitHub Release。FT-02/03 各自 tag 的 docker job 因 #13 `mst2-codec` path-dep 红，前滚 FIX 由 v0.11.0 job `35258572677` 证明） | 既有 `POST /api/v1/delete-entry` / `POST /api/v1/move-entry` 扩 `is_directory`（默认 true=目录；`false`=Blob/BlobExecutable）；`GET /api/v1/tags/list` 为唯一 list（必填 `page`/`per_page`/`path`，POST 405，`per_page=0`→400）；`GET|DELETE /api/v1/tags/{name}?path=` 与 `mega_tag.path` 迁移，查找键 `(path, name)`；契约页「Libra pin」重钉 v0.11.3。关闭 60917 `DEFER-LB-01/02/03` 与 `DEFER-LB-11` 隔离面（覆写 DEFER-LB-02 双方法窗口为 GET-only）。不覆盖：tag `target` commit 归属（DEFER-FT-02 / DEFER-LB-11 剩余面）、空目录-only 删除（DEFER-LB-04）、`search_tree_for_update` 类型化 400（DEFER-LB-07）、README 产品写列表（DEFER-LB-08）、SQL `/` 边界（DEFER-LB-09）、读 tree 与落地窗口（DEFER-LB-10）、blob/LFS/Media GC（DEFER-FT-01）、Libra 客户端（DEP-FT-01 outgoing） |
| [`plan-20260919.md`](plan-20260919.md) | PT-09 / SB-02（出站拆除切片） | **已完成**（RM-01..RM-04；末卡文档；crate 停在 `0.38.1` / `eaaa59c`） | 本仓出站只留 `[notification.webhook]`（外加无关的 `[storage_events]`）。拆除 website-mail / Slack / in-app inbox / locale / 协作 UI（评论改删与 emoji、评审、labels/assignees、CLA）。不实现 PT-09 剩余的 build 触发器 / 次渠道 retry / 多实例矩阵。不改 website frontend。 |
| [`plan-20260916.md`](plan-20260916.md) | N/A（monorepo 路径 → GitHub 出站基础设施；非 Mega PT） | **已完成（2026-09-20）** | 24 张活动卡 `done`/`complete`；Q1–Q8 全 go。执行链路移交 [`plan-20260920.md`](plan-20260920.md)。`DEP-02` 已关闭（模板改回 Libra）。 |
| [`plan-20260920.md`](plan-20260920.md) | N/A（出站同步执行；承接 60916 `DEP-01`）；outgoing `DEFER-OX-02` → PT-15、`DEFER-OX-03` → PT-16 | **设计修订中，任务验收未完成** | OX 任务交付 outbox/worker/reported pack、Q7 operator 接口与 fake-client、GitHub live 门及新 tag 的 Docker D 证据；旧 `v0.42.25` 镜像精确补发和外部 AI 实际执行由 PT-15/16 独立接收，均保持 open。不改直播 ACK。 |
| [`plan-20260923.md`](plan-20260923.md) | N/A（首次使用路径策略与 ImportRepo 生命周期；非 Mega PT，不新增 PT 编号） | **已完成**（FU-01..FU-23 全部 done/complete，收口 v0.40.12 / `4bf6cc2`；「计划完成门」于 2026-09-26 在 v0.40.13 最终树补跑通过） | GitHub issues #25–#29：#25/#29 路径策略与开通（`root_dirs` / `import_dir` 形状校验、路径策略错误域、首推策略、`mega2 path provision`）；#28 合成 commit 签名头与帧；#26 ImportRepo 增量推送（ref CAS、只写 ref 的更新、存量别名路径规范化）；#27 ImportRepo 清理（台账、原子 detach、元数据清扫、写路径存活栅栏、`POST /api/v1/import-repo/remove`、运维 CLI `mega2 import-repo remove`）。长期剩余见该计划 `DEFER-FU-*`（无对象 GC、SIGTERM 等信号、类型化可重试错误、无分支存量挂载等）。 |
| [`plan-20261002.md`](plan-20261002.md) | PT-14（trunk 历史只读投影 P0） | **已完成** | 36 张任务卡已完成；34 个独立版本均有 tag 与非草稿 GitHub Release；六张卡的历史 Docker D 组绿灯保留，其余 29 张发布卡依 `EX-HP-01` 延期，Docker 发布不作为本计划完成证据；P1–P3 留在 PT-14。 |
| [`plan-20261015.md`](plan-20261015.md) | PT-17（Scoped Traces与Agent历史接口） | MTR100源码、A/B、自身与最终双审、0.42.60完整C、签名提交、main/tag/人工Release及适用D已实际闭合：done/complete。Mega6/109（活动6/108）、Libra0/26（活动0/25），联合6/135（活动6/133）。DEBT-MTR-01仅disabled HTTP94+SSH100拒绝边界closed；enabled/H1/LTR01仍待。 **历史索引：** FIX08已done/complete，Mega完成5/109（活动5/108）、Libra0/26（活动0/25）。MTR100六非Cargo路径已恢复，现in-progress；locally-accepted仅保留历史A/B输入资格，新组合树九门A及完整18项smoke未执行，新自身Review与0.42.60完整C/D待。 **历史索引：** MTR94 done/complete；计划未完成 | FIX08已完成源码、A/B、真实ownER05与最终交付审查、本地五门C、精确签名+DCO提交、main/tag/人工Release及适用D：done/complete。Mega正式完成5/109（活动5/108）、Libra仍0/26（活动0/25）。MTR100仅其FIX08发布前驱解除，可按受保护前像精确恢复六个非Cargo路径；恢复尚未在本收据执行，MTR100自身输入/Review/0.42.60完整C/D仍待，不能据此标本卡完成。 **此前阶段索引：** FIX08源码、六门A/B及ownER05双PASS已核；0.42.59最终五门本地C实际通过（session68149 exit0，20目标2981通过/0失败/3原有ignored/0过滤/0measured，fmt/strict clippy及双build零warning/error）。Lifecycle=in-progress、Acceptance=locally-accepted；尚未提交/推送/tag/人工Release/D，不能done/complete。MTR100 WIP仍受保护；Libra仍待新上游资格/H1/LTR01。 **此前阶段索引：** FIX08 in-progress/locally-accepted；2AC/3VER及六门A/B已核，自身ER05真实Claude/Codex双PASS（session82523 exit0）；版本入口实际session25405 exit0，58→59两面一致。完整C尚未启动；签名提交、推送/tag/Release/D均待。MTR100 WIP继续受保护，未恢复；Mega完成仍4/109、Libra0/26。 **历史阶段索引：** FIX08六门A/B原件已核（2026-10-21T10:23:18.538031+00:00）：in-progress/locally-accepted，2AC/3VER已核；ownER05、版本bump、完整C、签名提交、推送/tag/Release/D均未执行。MTR100 blocked/locally-accepted，受保护WIP尚未恢复；完成计数仍Mega4/109（活动4/108）、Libra0/26（活动0/25）。 **此前索引快照：** FIX08采纳候选待双审；Mega109/活动108、Libra26/活动25、联合135/活动133；完成仍4/109与0/26。D1入口STOP、未运行测试且claim已消费；MTR100 blocked/locally-accepted，独立FIX08完整C/D前不放行。 **此前索引快照（保留历史）：** MTR94 v0.42.58完整C/D已核、done/complete；93继承精确94覆盖而done/complete（来源限94/100）；01仅Mega覆盖完成、Libra仍待LTR01。Docker双架构/manifest及Harbor实际success。Libra本地signed b157396、未push，H1/LTR01仍待；100已核ER01/02/03并开始实现，验收/双审/C/D待。 **历史D待定索引：** MTR94五门与最终双审PASS；signed04c42e1/main/v0.42.58/人工Release已核，实际Docker39539957757及Harbor39539882313 D待，remote-pending；93覆盖/100前驱未闭合。Libra10658/network3与signed9314a17已核、未push，H1/LTR01仍待。 后继顺序与原失败见日期计划。 |

## 已替代 / 不采纳 / 已实现摘要

### 已替代

- 当前无 PT 被替代。若未来合并移植项，保留原 ID，并在此记录替代 PT 与理由。

### 不采纳

- **不把 moon 前端代码列为移植项。** 非 Rust 资产，mega2 仓库不承载前端代码；mega2 前端为 monoui `apps/next-app`，两个前端体系的功能一致性由 PT-12 治理（见"不进入"章节与 PT-12）。
- **不回流 Mega 的旧 notification/email/config 实现。** mega2 对应模块已是功能超集（`docs/refactoring/{notification,mail,config}.md`），只补缺口不重复移植（原则 8）。
- **不把"与 Mega 目录结构一致"作为目标。** mega2 的 contract 归并、config 提升、workspace 拆分、Vault 以 crates.io `libvault` + 集成层为事实源（非 vendored fork、非 Mega `libvault-core` 目录镜像）是已决架构改进，不因结构差异回退（原则 2；依赖形态见 [`plan-20260820.md`](plan-20260820.md)）。

### 已实现

- **PT-01** 已完成（实现面 + 文档收口 v0.1.176 + D 组 v0.1.177）。其余 PT-02..PT-12 仍未全部满足完成判据。**PT-13**（mega2 原生，统一推送密文）为候选，方案已写入本文，无日期计划。已完成的基础移植（callisto/jupiter/migration、contract 归并、config、vault、notification、orbit、协议止血、bellatrix）已在"当前基础"据实记录；本仓 chat/Notes 与 SMTP 邮件投递已按 plan-20260731 退场，不再列入当前基础。

## 路线图维护

- 每次审计先用 libra 核对 Mega 与 mega2 的实际 checkout revision 和工作区状态；dirty 或核对失败必须按实际 revision 记录，不把未核对的版本描述为最新。
- 每季度、重大架构变更或日期计划完成后，重新读取当前 `src/`、migration、API 路由和相关测试；不得复制上次"当前基础"文字代替复核。
- PT 编号一经被执行计划或 issue 引用，不重新编号；废弃项使用"已替代"或"不采纳"，并记录替代项、理由和证据。
- 新候选移植项必须同时给出 Mega revision/path、mega2 代码/测试缺口、价值、风险、依赖和最小可验证切入点。mega2 原生项（PT-13 起）Mega 列写 `N/A`，其余字段同样强制。
- 某项进入日期计划时，仅更新总览状态、链接和剩余长期缺口；详细章节不复制 owner、日期或任务列表。
- 移植完成只以当前可发布代码、测试、兼容/用户文档和迁移证据为准；本文文字、日期计划完成声明或 checkbox 不是完成证明。
- Mega 是移植目标与契约基线：接口、数据模型、安全边界和兼容策略默认与 Mega 对齐；任何偏离（含既有架构改进之外的新偏离）必须经 mega2 自身 RFC/ADR 显式决策并记录。


### MTR-91 当前来源审计登记（2026-10-20 08:33:39 UTC）

**历史MTR91追加：** 当前源码六来源审计A/B已核，`in-progress / locally-accepted`；本卡新双ER05与精确签名本地提交尚待，未授01/38消费。证据见[日期计划](plan-20261015.md#mtr-91-当前来源资格收据2026-10-20)。C/D继承MTR-38，未done/complete；旧FAIL/STOP及28卡状态不变。本条为MTR91自有追加，既有治理修订归MTR01。

## Scoped Traces 当前前滚候选索引（2026-10-20）

MTR38/91 done/complete；01原批次Mega C/D闭合，本次五文档amendment自身B/联合双审及精确签名本地提交已完成、94覆盖待、Libra三文档待LTR01。93已17AC/17VER审计核验、in-progress/locally-accepted（最终B已PASS），自身ER05双PASS及签名交接已核，受限source-qualified仅94/100；94已源码实现且A/B通过、in-progress/locally-accepted，自身ER05及C/D待；95～101与FIX-MTR-06/07仍pending/空，108/26总数、107/25活动；R2新双审已PASS，不沿用97/26 R5a；两仓治理精确本地提交已验签、93自身审查/交接已核，94等待自身双审与最终C/D。受审执行顺序93→94→100→FIX-MTR-06→95→96→97→FIX-MTR-07→98→101→99→11→40→13→39→92→12/H1。93 carrier94；新增FIX-MTR-06/07分别阻断95的native绑定与98的raw不可变创建，94/100不消费这两处写入。其余新代码独立patch及完整C/D。94 HTTP禁用、100 SSH禁用分别实际完成后才关闭DEBT-MTR-01；96结构、97资源、98identity、101逐commit保护、99raw流、11pack编码职责分开，11原8AC/39原6AC保留。见[日期计划](plan-20261015.md)。新增治理由94未来覆盖，不借38旧C/D。


**MTR93消费边界：** 93自身双审及精确signed+DCO交接已核，source-qualified仅94/100的disabled拒绝路径；94已A/B及自身R2双PASS，最终C2及已授权一次C3均FAIL/STOP；C3 claim已消费，C4新方案双审及授权尚待，C/D未闭合。MTR47 rebind身份/根锁两项与MTR06 move恢复风险具名阻断95及后续身份消费，FIX06不是充分保证；缺消费条款的96/FIX06/07/11/39/92/12须先ER03及真实双审。53项明细与原件SHA见[日期计划](plan-20261015.md)本次正式审计收据。三次审计脚本格式校验失败保留，v4实际exit0；无新产品测试或旧C2重试。

<a id="mtr94-c4-local-gates-20261021"></a>

## MTR94 C4本地五门实际终态与提交前事实同步（2026-10-21T03:14:24.621527+00:00）

MTR94源码/A-B及自身R2真实双PASS保留；新授权C4一次120分钟已实际执行，0.42.58最终源码树五门均exit0、编译0警告/0错误；full20目标2979PASS/0FAIL/3原有ignored、无过滤。窗口2082.320秒，98530输入终态同一，owned host为空/Git容器已停。当前in-progress/locally-accepted；后置文档最终双审、精确签名提交、main/tag/Release和实际D待，未填done/complete。原C2/C3 FAIL/STOP、消耗claim及历史C2原因未知保留；不复用剩余时间。

Libra选定上游3b1200a、本地2332823f1a2841a4e3121b164965dea25dea98fc/0.30.69；新树fmt/clippy/CLI build/full10658/network3实际exit0，21原有ignored、1 flaky与proc-macro-error2 future-incompat警告保留；后置三文档最终R2真实双PASS，signed+DCO2332823及精确3路径/签名已核，仅本地。此次两仓新事实文档另作B/双审；H1/LIVE/WRITERS与Worktree发布冻结不变，LTR01唯一发布点、formal0/26，未push。

本次实际命令、UTC、SHA与限制见[日期计划终态登记](plan-20261015.md#mtr94-c4-local-gates-20261021)。

ER01/02/03：本次只在实际C4终态之后更新既有计划文档写集；代码/配置/测试/版本/协议语义/依赖均不变，134卡/132活动/223边保持。原测试只覆盖其冻结输入，新文档须自身B和实际Claude/隔离Codex最终双审；不把新字节倒记为旧测试或旧PASS。最终发布仍串行，MTR94精确signed+DCO/main/标注v0.42.58/人工Release/实际D均须另核；Libra仅精确本地文档提交，C/D由LTR01承载，不提前push。93→94→100与H1/client前驱保持；FIX3/LTR00取消、3/3诊断和8/8 A/B STOP、G13退出与所有旧失败不变。

当前为提交前快照。root具名负责在实际最终双审、提交、推送和D之后按真实终态事实同步，不以候选或文件存在当完成；若远端变化，先精确整合并按变更范围复验，不force或搬运扩大其它发布权限。

### 本次覆盖前的带日期历史页首（原事实保留）

> **历史发布执行状态（2026-10-20T16:47:42.108349+00:00）：** MTR94源码/A-B及自身R2双PASS保留；用户授权的一次120分钟C3已实际消费并FAIL/STOP。fmt与严格clippy exit0，full-test exit101：library1349通过/1337失败/3既有ignored，未到后续targets/build/build-tests。实际131.308秒，非超时；专属host为空/Git容器已停，原shared-daemon unresolved保守标记保留。blocked/locally-accepted，未提交/push/tag/Release/D；禁止自动重跑或借剩余时间重新执行。
> **历史跨仓事实（2026-10-20T16:56:43.319120+00:00）：** 已原生整合选定最新远端3b1200a80e00e8d1a8866ab07aa790c27f8a17a9（含e3eaff1源码变更），本地HEAD 3eec95934e5862f2528c6fb181fcd5da3b7a4ab3；5本地文档提交逐条重放重签、DCO/GPG核同，源与上游相同仅3计划文档本地差异。版本四面0.30.69；新CLI/新树完整资格待，不继承旧10636集合。原e4d树022恢复16/full10636/network3真实PASS及后置双审签名870aec0保留为历史选定树资格。未push/tag/Release/D，H1/LIVE/LTR01仍待。


<a id="mtr94-publication-20261021"></a>

## MTR94实际提交、发布与远端门登记（2026-10-21T03:43:53.672324+00:00）

MTR94 v0.42.58完整C/D已核、done/complete；93继承精确94覆盖而done/complete（来源限94/100）；01仅Mega覆盖完成、Libra仍待LTR01。Docker双架构/manifest及Harbor实际success。Libra本地signed b157396、未push，H1/LTR01仍待；100未开工，下一步重核ER01/02/03。

> **历史D待定阶段：** MTR94源码/A-B、自身R2及C4后置最终真实Claude/Codex双PASS已核；C4五门exit0、编译0警告/错误，full2979通过/0失败/3原有ignored（20目标）。精确13路径signed+DCO提交`04c42e10ba88ed45b830d509407d5732f4f17845`已推main；标注`v0.42.58`及人工Release已核。版本Docker run39539957757与main Harbor run39539882313均已触发，最后观察运行中，D尚待；94为in-progress/remote-pending，93覆盖及100前驱未闭合，100未开工。


Libra选定上游`3b1200a80e00e8d1a8866ab07aa790c27f8a17a9`、0.30.69资格保留：full10658与network3实际PASS，21原有ignored、1 flaky及future-incompat警告不隐去；三份计划文档精确signed+DCO提交`9314a17348aceb0c631139715fd213ead73ed90c`已验签，仅本地未push。H1/LIVE/WRITERS与Worktree发布冻结、LTR01唯一发布点均不变，formal0/26。

具体签名、双审、发布原件、D事件映射及P2处置见[日期计划发布登记](plan-20261015.md#mtr94-publication-20261021)。

### 历史索引中间行快照（原文字完整保留，含前轮新增现态前缀）

> | [`plan-20261015.md`](plan-20261015.md) | PT-17（Scoped Traces与Agent历史接口） | MTR94五门与最终双审PASS，signed04c42e1/main/标注v0.42.58/人工Release已核；实际Docker39539957757及Harbor39539882313 D待，remote-pending，93覆盖与100前驱未闭合；Libra资格10658/network3及signed9314a17已核、未push，H1/LTR01仍待。本次后置事实文档另做B/双审。 **历史提交前索引：** 当前C4本地五门通过，2979/0/3、20目标，0.42.58；最终输入审查/签名提交/发布C-D待，93覆盖与100前驱仍未解除；Libra10658/network3与本地signed2332823已核，H1/LTR01仍待。 以下为历史阶段快照： MTR94源码与A/B已通过（HTTP3/3、模块8/8、API433/0/1既有ignored、release Git smoke17/17、fmt/clippy与release build退出0），in-progress/locally-accepted；自身ER05及完整C/D待，未提交/推送。93受限source-qualified仅94/100且C/D94待；Libra已整合e4d843d至e2509aa，新树full/network待，H1未交付。 **历史12:26记账：** MTR93的17AC/17VER、文档B、自身真实Claude/Codex双PASS和精确signed+DCO交接已核；Mega `2320e95a80b5d813534d9ce8696baff1b7aab51f`、Libra `79f7cfaddbc72070a9b8d2a42c06410387c07fef`均仅本地未push。93为source-qualified（仅94/100拒绝路径）、in-progress/locally-accepted，C/D94待。MTR94于12:22:51 UTC完成ER01/02/03正式开工，in-progress/空，源码实现中；6AC/1VER全部未勾，验收/自身双审/版本及C/D均未执行。38/91 done/complete，01/93本增量Mega覆盖待94，Libra待LTR01；H1未交付，旧53风险/28卡blocked/取消及FAIL/STOP保持。总108/26、活动107/25。 **历史索引输入：** MTR-38 A/B已通过（resolver8/8、v0 3/3、v2 2/2、traces模块9/9、真实Git smoke17/17、nightly fmt与strict clippy exit0），当前in-progress/locally-accepted；自身Claude/Codex ER05待，尚未bump/commit/push/tag/Release/C-D。MTR91/01仅source-qualified；Libra LTR10仍等H1/LIVE，取消验收与旧FAIL/STOP保留。 历史登记： 当前 2026-10-20 08:57:23 UTC：MTR-91与MTR-01均已A/B、各自Claude/Codex双PASS和精确签名本地提交，source-qualified；MTR-38已开工，三源码首版待真实验收/自身双审；尚未bump/push/tag/Release/C-D。Mega2 91=3723200、01=1385f58；Libra01=32287f7（上游141de9ab）。Libra LTR-10仍等H1/LIVE，FIX3/LTR00取消及旧FAIL/STOP不变。 历史登记： 当前 2026-10-20 08:43:28 UTC：H0 R5a真实Claude/Codex双PASS；MTR-91自身双PASS、签名+DCO本地提交`3723200180b914f20af7152968438009cac373fe`已核，source-qualified；MTR-01本轮A/B已核、in-progress/locally-accepted，自身ER05及两仓精确本地提交待取得，之后才放行MTR-38。91/01均未C/D完成；Libra已原生快进141de9ab，LTR-10仍等H1/LIVE。 历史： **历史取消快照 2026-10-20 07:20:41 UTC：Libra main=origin/main=6003a0c，取消前 source pin 三行漂移、旧静态脚本 exit1；FIX3/LTR00 验收取消，引用方继续阻塞，替代依赖待两仓双审。历史 2026-10-20 06:36:12 UTC：FIX3/LTR00 验收取消，DEFER-TR-FIX3-AC/DEFER-LTR-06；两卡 blocked/空、引用方不自动解锁，替代依赖待两仓双审。历史 2026-10-20：Libra `e24c2d4`、Mega2 本地 `74ce530d`，Mega2 远端 SSH fetch 未核实；原 C2 材料不存在，停止原件诊断；R15/R16/R17 双 FAIL 均保留；R18 冻结输入治理修订 Claude/Codex 双 PASS（仅治理）；Mega2 远端未核实，H0 仍未 PASS；FIX3/FIX4/MTR-01/LTR00 blocked/空，LTR01 未开工。历史 2026-10-20 03:44:17 UTC：Libra `fc56869`、Mega2 `74ce530d`；R13 Claude/Codex 双 FAIL 的 H0 映射旧状态 P1 已前滚，R14 双审待取得；无包 STOP、旧 B8/3+8 与 FIX3/LTR00 blocked 不变。历史 2026-10-20 03:39:25 UTC：Libra `fc56869`、Mega2 `74ce530d`；R12 Claude FAIL/Codex PASS 的旧快照 P1 已修，R13 双审待取得；无包 STOP、旧 B8/3+8 与 FIX3/LTR00 blocked 不变。历史 2026-10-20 03:31:42 UTC：Libra `fc56869`、Mega2 `74ce530d`；R11 旧 `ec77730` 输入双 PASS；用户确认无交接包，FIX3 同源诊断 STOP，旧 B8/3+8 保留；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-01 未开工；历史取消快照 H0 新审待取得。历史 2026-10-20 00:38:10 UTC：Libra `main=ec77730a`、Mega2 `main=74ce530d`；R10 Claude FAIL/Codex PASS：两计划正文 DEP-TR-H0 行仍以旧 R6 为历史取消快照的 P1 已前滚，R11 终态由本字节的真实隔离双审输出裁定，不预填 PASS。FIX3 原 B8 FAIL/3+8 STOP 与 C2 原件缺失不变；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-00 AC-3/自身 ER-05 未过、formal 0/26，LTR-01 未开工；零新增诊断/A-B 窗或发布。 历史 2026-10-20 00:34 UTC：新 HEAD `ec77730` 的 49 路径/444 链接静态门通过；R9 Claude FAIL/Codex PASS 的索引与修订历史 P1 已前滚，R10 待双审。FIX3、FIX4、MTR-01 与 LTR-00 仍 blocked/空，零新诊断/A-B 窗；LTR-01 未开工。 历史 2026-10-19 18:34 UTC：新 FIX3 条件治理终态 R6 Claude/Codex 双 VERDICT: PASS（治理范围）；Claude 两项 P2 已具名修正：Libra R5 轮次加“新提案”限定、LTR00 B 静态摘要补 AC-4。旧失败/STOP、零新窗口及所有卡阻塞不变；本次纯文档修订的结论以隔离终态审查输出为准。** 历史：**2026-10-19 18:30 UTC：新 FIX3 条件恢复治理 R5 Claude/Codex 双 VERDICT: PASS（冻结输入，仅治理）；收据与 H0/LTR00 B 静态限定已前滚记账，终态文档待独立复核。原 C2 包、精确诊断合同、新窗口及交叉写者让出仍缺；FIX3/MTR-01/FIX4/LTR00 blocked/空，无卡验收、提交或发布授权。** 历史：**2026-10-19 18:25 UTC：新 FIX3 提案 R4 Claude/Codex 双 FAIL；LTR-00 现行 AC-1/AC-2/VER-2 曾写“待 R3”与已发生 R3 冲突，三处已改为不依赖轮次的“现行 H0 待双 PASS”；R5 待审。原 C2 包与精确诊断合同仍缺，零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空。** 历史：**2026-10-19 18:21 UTC：新 FIX3 提案 R3 Claude FAIL/Codex PASS；Claude P1 为 Libra 生效计划总览仍以旧 18:08 作为历史取消快照，已修；审查 R4 待双 PASS。恢复方案仍须原 C2 包与另审精确诊断合同；零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空。 历史 2026-10-19 18:16 UTC：新 FIX3 提案 R2 Claude/Codex 均 FAIL；原全量约62分钟及连接压力与拟定60分钟诊断上限冲突、Mega2 活跃索引仍称旧 R6 历史取消快照，两项 P1 已前滚修订，R3 待审。旧 B8 FAIL、3/3、8/8 STOP 保留；零新诊断/A-B 授权，FIX3/MTR-01/FIX4 与 LTR-00 blocked/空。 历史 2026-10-19 18:08 UTC：FIX3 提案联合首轮 Claude PASS/Codex FAIL，生效索引 P1 已修、第二轮待审；零新窗，LTR-00 与 FIX3 链 blocked。** 历史：**2026-10-19 18:02 UTC：提案待审、LTR-00 自身双 FAIL。** 历史：**2026-10-19 17:39:15 UTC：H0 R6 双 PASS，当时仅 LTR-00 治理审计可继续。** 历史：**2026-10-19 17:33:08 UTC：H0 R5 Claude FAIL/Codex PASS，日期索引旧下一卡已修，R6待审。** 历史：**2026-10-19 17:28:17 UTC：H0 R4 Claude FAIL/Codex PASS，PT-17主表旧下一卡已修，R5待审。** 历史：**2026-10-19 17:21:13 UTC：H0 R3 Claude FAIL/Codex PASS；四P1前滚修订后R4待审，FIX3/MTR-01/FIX4 blocked/空，其他27张依赖后继及MTR-10 blocked/complete。** 历史：**2026-10-19 17:12:27 UTC：H0 R2双FAIL已修订待R3；FIX3/MTR-01 blocked/空，其他27张依赖后继及MTR-10 blocked/complete。** 历史：**2026-10-19 17:02:50 UTC：MTR-10 v0.42.56 tag Docker 三job success，历史取消快照blocked/complete，FIX3仍blocked/空；H0修订后双审待。** 历史：**2026-10-19 16:59:27 UTC：H0首审双FAIL，FIX3原AC-1/VER-5未证而blocked；28张依赖后继blocked/complete，MTR-10 blocked/remote-pending，已发布版本保留。** 历史：**2026-10-17 22:58:26 UTC MTR-77 done/complete，0.42.37，`707ddd8`，Docker run `39076153505` success，下一张卡 MTR-78** 历史：**2026-10-17 22:40:18 UTC MTR-77 in-progress/locally-accepted，0.42.37，Docker D 当时未取得** 历史：**2026-10-17 21:53:03 UTC MTR-76 done/complete，0.42.36，`87eed19`，Docker run `39071597649` success** 历史：**2026-10-17 21:33:14 UTC MTR-76 in-progress/locally-accepted，0.42.36，Docker D 当时未取得** 历史：**2026-10-17 20:43:41 UTC MTR-50 done/complete，0.42.35，`e8bc7cb`，Docker run `39066690076` success** 历史：**2026-10-17 20:22:35 UTC MTR-50 in-progress/locally-accepted，0.42.35，Docker D 当时未取得** 历史：**2026-10-17 19:11:57 UTC MTR-04 done/complete，0.42.34，`ce1fe9d`，Docker run `39060072328` success** | MTR94源码/A-B及修复后自身R2双PASS保留；最终C2失败、原因未证，blocked/locally-accepted。C3条件方案R3 Claude PASS/Codex FAIL，输入路径集合与超时进程清理两P1待修；新候选未获审、更未获用户60分钟窗口，禁止执行。未提交/push/tag/Release/D，93覆盖与100前驱仍待。 Libra原full exit100（10620通过/16失败/21既有ignored）、network exit0（3/3）不变。恢复R3对Libra v3单项实际双PASS；组合总体Claude PASS/Codex FAIL（Mega脚本两P1），未执行。现拆为独立Libra恢复R4审查与入口，脚本v3不改、只按本次精确manifest绑定。H1/LIVE仍缺，LTR01与Worktree发布边界保持。 **历史12:26记账：** MTR93的17AC/17VER、文档B、自身真实Claude/Codex双PASS和精确signed+DCO交接已核；Mega `2320e95a80b5d813534d9ce8696baff1b7aab51f`、Libra `79f7cfaddbc72070a9b8d2a42c06410387c07fef`均仅本地未push。93为source-qualified（仅94/100拒绝路径）、in-progress/locally-accepted，C/D94待。MTR94于12:22:51 UTC完成ER01/02/03正式开工，in-progress/空，源码实现中；6AC/1VER全部未勾，验收/自身双审/版本及C/D均未执行。38/91 done/complete，01/93本增量Mega覆盖待94，Libra待LTR01；H1未交付，旧53风险/28卡blocked/取消及FAIL/STOP保持。总108/26、活动107/25。 **历史索引输入：** MTR-38 A/B已通过（resolver8/8、v0 3/3、v2 2/2、traces模块9/9、真实Git smoke17/17、nightly fmt与strict clippy exit0），当前in-progress/locally-accepted；自身Claude/Codex ER05待，尚未bump/commit/push/tag/Release/C-D。MTR91/01仅source-qualified；Libra LTR10仍等H1/LIVE，取消验收与旧FAIL/STOP保留。 历史登记： 当前 2026-10-20 08:57:23 UTC：MTR-91与MTR-01均已A/B、各自Claude/Codex双PASS和精确签名本地提交，source-qualified；MTR-38已开工，三源码首版待真实验收/自身双审；尚未bump/push/tag/Release/C-D。Mega2 91=3723200、01=1385f58；Libra01=32287f7（上游141de9ab）。Libra LTR-10仍等H1/LIVE，FIX3/LTR00取消及旧FAIL/STOP不变。 历史登记： 当前 2026-10-20 08:43:28 UTC：H0 R5a真实Claude/Codex双PASS；MTR-91自身双PASS、签名+DCO本地提交`3723200180b914f20af7152968438009cac373fe`已核，source-qualified；MTR-01本轮A/B已核、in-progress/locally-accepted，自身ER05及两仓精确本地提交待取得，之后才放行MTR-38。91/01均未C/D完成；Libra已原生快进141de9ab，LTR-10仍等H1/LIVE。 历史： **历史取消快照 2026-10-20 07:20:41 UTC：Libra main=origin/main=6003a0c，取消前 source pin 三行漂移、旧静态脚本 exit1；FIX3/LTR00 验收取消，引用方继续阻塞，替代依赖待两仓双审。历史 2026-10-20 06:36:12 UTC：FIX3/LTR00 验收取消，DEFER-TR-FIX3-AC/DEFER-LTR-06；两卡 blocked/空、引用方不自动解锁，替代依赖待两仓双审。历史 2026-10-20：Libra `e24c2d4`、Mega2 本地 `74ce530d`，Mega2 远端 SSH fetch 未核实；原 C2 材料不存在，停止原件诊断；R15/R16/R17 双 FAIL 均保留；R18 冻结输入治理修订 Claude/Codex 双 PASS（仅治理）；Mega2 远端未核实，H0 仍未 PASS；FIX3/FIX4/MTR-01/LTR00 blocked/空，LTR01 未开工。历史 2026-10-20 03:44:17 UTC：Libra `fc56869`、Mega2 `74ce530d`；R13 Claude/Codex 双 FAIL 的 H0 映射旧状态 P1 已前滚，R14 双审待取得；无包 STOP、旧 B8/3+8 与 FIX3/LTR00 blocked 不变。历史 2026-10-20 03:39:25 UTC：Libra `fc56869`、Mega2 `74ce530d`；R12 Claude FAIL/Codex PASS 的旧快照 P1 已修，R13 双审待取得；无包 STOP、旧 B8/3+8 与 FIX3/LTR00 blocked 不变。历史 2026-10-20 03:31:42 UTC：Libra `fc56869`、Mega2 `74ce530d`；R11 旧 `ec77730` 输入双 PASS；用户确认无交接包，FIX3 同源诊断 STOP，旧 B8/3+8 保留；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-01 未开工；历史取消快照 H0 新审待取得。历史 2026-10-20 00:38:10 UTC：Libra `main=ec77730a`、Mega2 `main=74ce530d`；R10 Claude FAIL/Codex PASS：两计划正文 DEP-TR-H0 行仍以旧 R6 为历史取消快照的 P1 已前滚，R11 终态由本字节的真实隔离双审输出裁定，不预填 PASS。FIX3 原 B8 FAIL/3+8 STOP 与 C2 原件缺失不变；FIX3/FIX4/MTR-01/LTR-00 blocked/空，LTR-00 AC-3/自身 ER-05 未过、formal 0/26，LTR-01 未开工；零新增诊断/A-B 窗或发布。 历史 2026-10-20 00:34 UTC：新 HEAD `ec77730` 的 49 路径/444 链接静态门通过；R9 Claude FAIL/Codex PASS 的索引与修订历史 P1 已前滚，R10 待双审。FIX3、FIX4、MTR-01 与 LTR-00 仍 blocked/空，零新诊断/A-B 窗；LTR-01 未开工。 历史 2026-10-19 18:34 UTC：新 FIX3 条件治理终态 R6 Claude/Codex 双 VERDICT: PASS（治理范围）；Claude 两项 P2 已具名修正：Libra R5 轮次加“新提案”限定、LTR00 B 静态摘要补 AC-4。旧失败/STOP、零新窗口及所有卡阻塞不变；本次纯文档修订的结论以隔离终态审查输出为准。** 历史：**2026-10-19 18:30 UTC：新 FIX3 条件恢复治理 R5 Claude/Codex 双 VERDICT: PASS（冻结输入，仅治理）；收据与 H0/LTR00 B 静态限定已前滚记账，终态文档待独立复核。原 C2 包、精确诊断合同、新窗口及交叉写者让出仍缺；FIX3/MTR-01/FIX4/LTR00 blocked/空，无卡验收、提交或发布授权。** 历史：**2026-10-19 18:25 UTC：新 FIX3 提案 R4 Claude/Codex 双 FAIL；LTR-00 现行 AC-1/AC-2/VER-2 曾写“待 R3”与已发生 R3 冲突，三处已改为不依赖轮次的“现行 H0 待双 PASS”；R5 待审。原 C2 包与精确诊断合同仍缺，零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空。** 历史：**2026-10-19 18:21 UTC：新 FIX3 提案 R3 Claude FAIL/Codex PASS；Claude P1 为 Libra 生效计划总览仍以旧 18:08 作为历史取消快照，已修；审查 R4 待双 PASS。恢复方案仍须原 C2 包与另审精确诊断合同；零新诊断/A-B 授权，LTR-00 与 FIX3 链 blocked/空。 历史 2026-10-19 18:16 UTC：新 FIX3 提案 R2 Claude/Codex 均 FAIL；原全量约62分钟及连接压力与拟定60分钟诊断上限冲突、Mega2 活跃索引仍称旧 R6 历史取消快照，两项 P1 已前滚修订，R3 待审。旧 B8 FAIL、3/3、8/8 STOP 保留；零新诊断/A-B 授权，FIX3/MTR-01/FIX4 与 LTR-00 blocked/空。 历史 2026-10-19 18:08 UTC：FIX3 提案联合首轮 Claude PASS/Codex FAIL，生效索引 P1 已修、第二轮待审；零新窗，LTR-00 与 FIX3 链 blocked。** 历史：**2026-10-19 18:02 UTC：提案待审、LTR-00 自身双 FAIL。** 历史：**2026-10-19 17:39:15 UTC：H0 R6 双 PASS，当时仅 LTR-00 治理审计可继续。** 历史：**2026-10-19 17:33:08 UTC：H0 R5 Claude FAIL/Codex PASS，日期索引旧下一卡已修，R6待审。** 历史：**2026-10-19 17:28:17 UTC：H0 R4 Claude FAIL/Codex PASS，PT-17主表旧下一卡已修，R5待审。** 历史：**2026-10-19 17:21:13 UTC：H0 R3 Claude FAIL/Codex PASS；四P1前滚修订后R4待审，FIX3/MTR-01/FIX4 blocked/空，其他27张依赖后继及MTR-10 blocked/complete。** 历史：**2026-10-19 17:12:27 UTC：H0 R2双FAIL已修订待R3；FIX3/MTR-01 blocked/空，其他27张依赖后继及MTR-10 blocked/complete。** 历史：**2026-10-19 17:02:50 UTC：MTR-10 v0.42.56 tag Docker 三job success，历史取消快照blocked/complete，FIX3仍blocked/空；H0修订后双审待。** 历史：**2026-10-19 16:59:27 UTC：H0首审双FAIL，FIX3原AC-1/VER-5未证而blocked；28张依赖后继blocked/complete，MTR-10 blocked/remote-pending，已发布版本保留。** 历史：**2026-10-17 22:58:26 UTC MTR-77 done/complete，0.42.37，`707ddd8`，Docker run `39076153505` success，下一张卡 MTR-78** 历史：**2026-10-17 22:40:18 UTC MTR-77 in-progress/locally-accepted，0.42.37，Docker D 当时未取得** 历史：**2026-10-17 21:53:03 UTC MTR-76 done/complete，0.42.36，`87eed19`，Docker run `39071597649` success** 历史：**2026-10-17 21:33:14 UTC MTR-76 in-progress/locally-accepted，0.42.36，Docker D 当时未取得** 历史：**2026-10-17 20:43:41 UTC MTR-50 done/complete，0.42.35，`e8bc7cb`，Docker run `39066690076` success** 历史：**2026-10-17 20:22:35 UTC MTR-50 in-progress/locally-accepted，0.42.35，Docker D 当时未取得** 历史：**2026-10-17 19:11:57 UTC MTR-04 done/complete，0.42.34，`ce1fe9d`，Docker run `39060072328` success** | 历史：2026-10-17 15:42:03 UTC：MTR-70 为 done/complete（`35d5457`，tag `v0.42.32`，Docker run `39045161292` success）。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-72。历史：MTR-03 为 done/complete（`959c1e2`，tag `v0.42.31`，Docker run `39039264618` success）。FIX-MTR-03 已发布 v0.42.28（done/complete，AC-1 与 VER-5 未勾选）。FIX-MTR-01 已发布 v0.42.29（`1dbfb0c`，Docker run `39014885765` success，done/complete）。MTR-02 为 done/complete（`b537ced`，tag `v0.42.30`，Docker run `39034027014` success）。MTR-01 继承该 C/D，为 done/complete。FIX-MTR-03 的 AC-1 与 VER-5 未勾选。下一张卡是 MTR-03。LTR-01 未开工。不推送 Libra。历史 2026-10-16 现场：origin/main=`b784111` 已推送，`REL-MTR-FIX-01` 退出；FIX3 预算与 STOP 不重置；FIX-MTR-01 与 FIX-MTR-03 改为各自 independent patch；FIX-MTR-04 不再发布；恢复窗已执行，AC-1 未勾选。历史迁机WIP检查点（2026-10-15T17:46:23.821690+00:00）：FIX3测试候选已取得受控旧10秒RED101、新180秒GREEN1/1、原exact1/1、默认module8/8及fmt/strict0；C2具体DB机制仍未知，第四oracle归属仅具名本地有界推断。B8于17:13:38..17:13:42 UTC实际exit1（native diff --check因引用空行尾空格exit2），STOP保留、3诊断/8AB额度耗尽，不重跑/重置/晋升；FIX3 blocked/空，ownER05/G13最终准入及FIX4完整C/D未取得，formal0/95与0/26。仅拟向handoff/scoped-traces-20261016保存未发布工作；0.42.27/main/tag/Release不变。新机先核OID/签名、整合上游并重做ER01–03和失败后的计划/验收，换机不续用旧窗口或STOP；本机不续后卡。 |


<a id="mtr94-d-complete-20261021"></a>

## MTR94实际D闭环与来源覆盖（2026-10-21T04:02:51.712358+00:00）

MTR94源码/A-B及自身真实双审、C4五门（2979通过/0失败/3原有ignored）、signed04c42e1、main/标注v0.42.58/人工Release已核；版本Docker run39539957757的amd64/arm64/manifest三job全部success，main Harbor run39539882313亦success。root于2026-10-21T04:00:24.847557+00:00重新取得原始API并核同提交/ref/Release。94现done/complete；93原signed2320e95精确树获94 C/D覆盖，done/complete，来源资格仍仅94/100拒绝路径；01本增量Mega覆盖已取得，Libra侧仍待LTR01，整卡不done。MTR100仍pending，须重新ER01/02/03后开工；本次D事实文档自身B/双审及精确提交待，不倒记为版本预发布字节。

Libra选定上游3b1200a/0.30.69，full10658/network3资格已核，21既有ignored、1 flaky、future-incompat警告保持；已签名本地文档checkpoint b157396f5c1aacbbaf5071b23ef34d9184f968e6（父9314a17），未push。H1/LIVE/WRITERS与Worktree冻结、LTR01唯一发布点不变。Mega正式完成4/108（活动4/107），Libra0/26（活动0/25）；其余旧blocked不批量改判。

完整D原件、精确来源覆盖与P2处置见[日期计划D闭环登记](plan-20261015.md#mtr94-d-complete-20261021)。


## MTR100开工与MTR94终态交接（2026-10-21T04:23:33.853724+00:00）

MTR94完整C/D及最终D事实双审已核；Mega文档收口c52bf4a已推main，Libra对应signed e938c05仅本地。93/94 done/complete，01仅Mega侧覆盖，Libra仍待LTR01；MTR100于2026-10-21T04:22:24.959231+00:00完成ER01/02/03，现in-progress/空，准备实现disabled SSH拒绝。5AC/1VER未验收、自身双审与独立patch完整C/D未执行；不开放enabled数据面，不重启旧FIX3/LTR00或C4窗口。

详细来源、原件与边界见[日期计划](plan-20261015.md)。
