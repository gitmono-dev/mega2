**现行 FIX18 C2根核STOP、清理已核、C3待审（2026-10-29 13:12 UTC）：** FIX19仍为已发布main `4986dbac`/`v0.42.79`，正式Mega27/127、Libra0/26；FIX18源码/A3/自身ER05 R2保留。C2执行审本地Claude八PASS/P0P1零，五原C命令均exit0且无警告：22目标3187通过/0失败/3既有ignored，双build通过；但独立根核R1因测试PID的exe inode采样缺失而exit1/STOP，C2完整本地C未通过。R2/R3/R4根恢复审查均FAIL且零执行，原1200秒根窗口已过，不转移余额。清理R2本地Claude六PASS/P0P1零、预检与单次执行exit0，精准删除25个本轮零连接测试库，独立后检确认原107库/五服务/源树不变；此清理明确不补算C2。下一门为受审前滚C3完整五命令+独立根核，候选版本0.42.80未提交/推送/tag/Release/D；MTR12/H1及Libra后继未解除。
> **历史（C2执行前摘要）：** **现行 FIX18 D2根核PASS、完整C2合同待审（2026-10-29）：** FIX19已发布`4986dbac/v0.42.79`且main/tag D成功，正式Mega27/127、Libra0/26不变；FIX18 RED1/A3/自身本机Claude ER05 R2 PASS保留。C1原全量SSH38/1导致`cargo test --all` exit101/STOP，双build未运行；C1新增21库受审精确清理并root核原107库/五服务保持。D1整组SSH测试stdout39/39但NULL观测器使控制器STOP，不追认PASS；D2本机Claude R1～R4各H0 FAIL/P1=1保留，R5六PASS/P0P1零且root采纳，D2控制器与独立根核实际exit0，整组39/39，102.280/900秒、原库/服务/二进制/进程/TMP/容器稳定；该结果不证明C1历史SIGINT根因，也不替代完整C。FIX18当前`blocked/locally-accepted`，仅C2完整合同待单独本机Claude审查，C2零claim；尚未签名提交/推送/tag/Release/CI-D。前沿仍FIX18→FIX20，Libra资格/LTR01发布不前移。
> **历史（D2根核前摘要）：** **现行 FIX18 C1与诊断D1均STOP、D2恢复待审（2026-10-29 11:08 UTC）：** FIX19已发布`4986dbac/v0.42.79`且main/tag D成功，正式Mega27/127、Libra0/26不变；FIX18 RED1/A3/自身本机Claude ER05 R2 PASS保留。C1 fmt/Clippy exit0、原全量SSH38/1使`cargo test --all` exit101，C1 STOP/双build未执行。恢复计划R1 FAIL/P1=3、R2 FAIL/P1=1、R3本机Claude七PASS/P0/P1=0均保留；受审精确清理仅删除C1新增21个零连接库，原107库/五服务不变。整组SSH诊断D1的测试进程实际39/39通过，但控制器因Postgres背景活动的`datname=NULL`在资源后检抛错并STOP，独立后检核库/服务/进程/TMP/SSH case/container均无残留；D1绝不记PASS或历史根因证明。现卡`blocked/locally-accepted`，仅一次修复NULL观察器的D2恢复候选待审，C2未claim；未提交/推送/tag/Release/D。D1原件`/tmp/fix-mtr18-C1-recovery-20261029/`，D2候选`/tmp/fix-mtr18-D2-20261029/`；Libra资格/LTR01发布边界与Mega后继FIX20均不前移。
> **历史（D1 STOP前摘要）：** **现行 FIX18 C1 STOP 与恢复待审（2026-10-29 10:28 UTC）：** FIX19仍已发布`4986dbac/v0.42.79`且main/tag D成功，正式Mega27/127、Libra0/26不变。FIX18 RED1、A3与自身本地Claude ER05 R2 PASS保留；0.42.80 C1于09:58:24Z认领，nightly fmt与严格Clippy均exit0，`source .env.test && cargo test --all`在原有`integration_git_ssh_trunk_token_anon_off_clone_fail`的迁移服务SIGINT退出断言处exit101（SSH 38通过/1失败）；双build未执行。C1控制器STOP，21个新增零连接测试库待受审精确清理，原库/五服务未变、无受控活进程；完整C、提交、推送、tag、Release及D均未取得。失败原因未证实，尚不能把本次SIGINT失败归因于或排除FIX18代码；历史C2根因仍未证明。现卡`blocked/locally-accepted`；一次诊断、精确清理和新C2仅为待本机Claude审查的恢复候选，旧C1失败与预算不重置。依赖前沿仍FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1；Libra LTR01发布及资格不前移。原件`/tmp/fix-mtr18-C1-20261029/`，terminal SHA256 `1312e4e77a27e90a3fe5cddbfc63014e4772589f72887974cc47a1573e385f21`、STOP后检SHA256 `8c45c7f639af25b798affc577b6371142749acc3021536edfb37ea2d3fcfb113`。
> **历史（C1 STOP前摘要）：** **现行 FIX18 自身ER05 R2 PASS、版本化C待（2026-10-29 09:16:57 UTC）：** FIX19已于`4986dbac/v0.42.79`完成签名发布及main/tag D；正式Mega27/127、Libra0/26。FIX18 RED1已核，A1操作STOP与A2过期零claim原件保留；A3单次窗口9新/5原TP13/1原信号、VER5、nightly fmt、严格Clippy和独立根核验均通过，根收据SHA `9944a8ce67d8e8bf3afc7517c6478dff6272cc732764c5adddd41f6820458c20`。自身ER05 R1 DOCS/ER05/VERDICT FAIL、P1=1及修复保留；R2本机Claude九PASS/P0/P1=0，root采纳SHA `4c6fba83a36e95442260ce862610bcdca446f232f25eec5293be5d2e1303c829`。FIX18仍in-progress/locally-accepted，源码未提交，0.42.80版本化完整C、main/tag远端D待。六Mega治理由FIX18最终C/D承载，root Libra三治理仅本地由LTR01承载；H0～H6及CL/B3/Import/Blake3限制不放宽。
> **历史（自身ER05 R2前摘要）：** **现行 FIX18 A3根核验通过、ER05与完整C/D待（2026-10-29 08:54:51 UTC）：** FIX19已于`4986dbac/v0.42.79`完成签名发布及main/tag D；正式Mega27/127、Libra0/26。FIX18 RED1已核，两旧converter panic及两SHA1负控；A1操作STOP、A2过期零claim均保留。A3计划R6六PASS、执行审R1/R2 FAIL与R3八PASS/P0/P1=0原件均保留；用户授权后08:43:45Z单次claim，控制器及独立根核验实际exit0，648.364/3600秒，9新测试/5原TP13/1原信号、VER5、nightly fmt、严格Clippy通过；107原测试库与服务不变，零新Mega测试库及受控活进程，收据`/tmp/fix-mtr18-A3-20261029/root-acceptance.json` SHA256 `9944a8ce67d8e8bf3afc7517c6478dff6272cc732764c5adddd41f6820458c20`。源码未提交，本卡in-progress/locally-accepted（A/B已核，ER05待），自己的ER05、版本化完整C、main/tag远端D均待；六Mega治理由本卡最终C/D承载，root Libra三治理仅本地由LTR01承载，不推送，H0～H6与CL/B3/Import/Blake3限制不放宽。
> **历史（A3根核验前摘要）：** **现行 FIX18 A3计划审R6 PASS、用户授权已记录、执行审待准入（2026-10-29 08:18:01 UTC）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z在工具输出观察（当时未同期落盘）原命令截止07:01:50.1295927Z已过，07:13:04Z正式收据重核A2零claim及双截止，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3计划审R1实际H0/HISTORY/VERDICT FAIL、P1=1，双仓修订列与预算账本已前滚，R2本机Claude六PASS/P0P1零仅覆盖其旧冻结字节；R3本机Claude H0/VERDICT FAIL、P1=1，表格其他现行单元格仍旧；现已逐格前滚20旧短语/7变体并为13先前修改补本格历史，R4本机Claude H0/VERDICT FAIL、P1=1发现protocol普通正文旧句；R5同样H0/VERDICT FAIL、P1=1发现九文件共用段旧“当前零claim”；两类正文均已前滚且原句逐处保留历史，R6版本化全段扫描与本机Claude六项PASS/P0/P1=0、root已采用；A3获用户授权仅一次3600秒（命令3000/根600）A窗口，用户已具名授权新一次A3 3600秒，R6计划审六PASS/P0/P1=0；待执行/来源/资源/根核验本机Claude八PASS与root准入后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。
> **历史（执行审R2前摘要）：** **现行 FIX18 A3计划审R6 PASS、用户授权已记录、执行审待准入（2026-10-29 08:18:01 UTC）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z在工具输出观察（当时未同期落盘）原命令截止07:01:50.1295927Z已过，07:13:04Z正式收据重核A2零claim及双截止，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3计划审R1实际H0/HISTORY/VERDICT FAIL、P1=1，双仓修订列与预算账本已前滚，R2本机Claude六PASS/P0P1零仅覆盖其旧冻结字节；R3本机Claude H0/VERDICT FAIL、P1=1，表格其他现行单元格仍旧；现已逐格前滚20旧短语/7变体并为13先前修改补本格历史，R4本机Claude H0/VERDICT FAIL、P1=1发现protocol普通正文旧句；R5同样H0/VERDICT FAIL、P1=1发现九文件共用段旧“当前零claim”；两类正文均已前滚且原句逐处保留历史，R6版本化全段扫描与本机Claude复审待；A3仅拟新增单次3600秒（命令3000/根600）A窗口，用户已具名授权新一次A3 3600秒，R6计划审六PASS/P0/P1=0；待执行/来源/资源/根核验本机Claude八PASS与root准入后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。
> **历史（A3授权前摘要）：** **现行 FIX18 A3治理R5 FAIL后九正文修复待R6（2026-10-29 08:06:40 UTC）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z在工具输出观察（当时未同期落盘）原命令截止07:01:50.1295927Z已过，07:13:04Z正式收据重核A2零claim及双截止，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3计划审R1实际H0/HISTORY/VERDICT FAIL、P1=1，双仓修订列与预算账本已前滚，R2本机Claude六PASS/P0P1零仅覆盖其旧冻结字节；R3本机Claude H0/VERDICT FAIL、P1=1，表格其他现行单元格仍旧；现已逐格前滚20旧短语/7变体并为13先前修改补本格历史，R4本机Claude H0/VERDICT FAIL、P1=1发现protocol普通正文旧句；R5同样H0/VERDICT FAIL、P1=1发现九文件共用段旧“当前零claim”；两类正文均已前滚且原句逐处保留历史，R6版本化全段扫描与本机Claude复审待；A3仅拟新增单次3600秒（命令3000/根600）A窗口，待用户具名授权及新ER03/H0/执行/来源/资源/根核验本机Claude PASS后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。

> **历史（A3治理R5前摘要）：** **现行 FIX18 A3治理R4 FAIL后正文修复待R5（2026-10-29 07:54:42 UTC）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z在工具输出观察（当时未同期落盘）原命令截止07:01:50.1295927Z已过，07:13:04Z正式收据重核A2零claim及双截止，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3计划审R1实际H0/HISTORY/VERDICT FAIL、P1=1，双仓修订列与预算账本已前滚，R2本机Claude六PASS/P0P1零仅覆盖其旧冻结字节；R3本机Claude H0/VERDICT FAIL、P1=1，表格其他现行单元格仍旧；现已逐格前滚20旧短语/7变体并为13先前修改补本格历史，R4本机Claude H0/VERDICT FAIL、P1=1发现protocol普通正文旧句；现已前滚并保留原文历史，仓外结构扫描九文件42297现行段/40格零旧RED短语（不冒充产品VER5）；R5待审；A3仅拟新增单次3600秒（命令3000/根600）A窗口，待用户具名授权及新ER03/H0/执行/来源/资源/根核验本机Claude PASS后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。

> **历史（A3治理R4前摘要）：** **现行 FIX18 A3治理R3 FAIL后逐格修复待R4（2026-10-29 07:41:54 UTC）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z在工具输出观察（当时未同期落盘）原命令截止07:01:50.1295927Z已过，07:13:04Z正式收据重核A2零claim及双截止，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3计划审R1实际H0/HISTORY/VERDICT FAIL、P1=1，双仓修订列与预算账本已前滚，R2本机Claude六PASS/P0P1零仅覆盖其旧冻结字节；R3本机Claude H0/VERDICT FAIL、P1=1，表格其他现行单元格仍旧；现已逐格前滚20旧短语/7变体并为13先前修改补本格历史，九文件现行旧语句零命中；R4待审；A3仅拟新增单次3600秒（命令3000/根600）A窗口，待用户具名授权及新ER03/H0/执行/来源/资源/根核验本机Claude PASS后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。

> **历史（A3治理R3前摘要）：** **现行 FIX18 A3治理P2收口待R3（2026-10-29 07:23:59 UTC）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z实查原命令截止07:01:50.1295927Z已过，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3计划审R1实际H0/HISTORY/VERDICT FAIL、P1=1，双仓修订列与预算账本已前滚，R2本机Claude六PASS/P0P1零仅覆盖其旧冻结字节；残余现行交叉引用已前滚，授权时效与过期收据边界已澄清，本轮R3待；A3仅拟新增单次3600秒（命令3000/根600）A窗口，待用户具名授权及新ER03/H0/执行/来源/资源/根核验本机Claude PASS后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。

> **历史（A3治理R2后P2收口前摘要）：** **现行 FIX18 A3治理R1 FAIL后的修订候选（2026-10-29T07:14:51Z）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z实查原命令截止07:01:50.1295927Z已过，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3计划审R1实际H0/HISTORY/VERDICT FAIL、P1=1，双仓修订列与预算账本已前滚，R2待；A3仅拟新增单次3600秒（命令3000/根600）A窗口，待用户具名授权及新ER03/H0/执行/来源/资源/根核验本机Claude PASS后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。

> **历史（A3治理R1前摘要）：** **现行 FIX18 A1 STOP、A2过期与新窗口待授权（2026-10-29）：** FIX19在4986dbac/v0.42.79的源码、验收、自身Review、签名提交推送、Release及main/tag D均已闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18 RED1已实核两SHA1负控与两旧converter panic，root633.766/2400秒；三生产文件与九同模块测试仍为未GREEN候选。A1经本机Claude5399b119六PASS后因root操作脚本错把安装清单文件SHA与内嵌Libra binary SHA混用，脚本exit1却误启动缓存复制，原81992/26808f exit1/STOP和159.549秒资源后检保留；无产品命令、107原DB与服务不变、零新DB/owned-live/TMP。A2同原窗恢复经本机Claude session385eb66d八PASS/P0=P1=0，06:28:20Z终态；root07:03:51Z实查原命令截止07:01:50.1295927Z已过，A2零claim/零测试且不得启动；原总截止07:11:50.1295927Z不扩。FIX18现blocked/Acceptance空；A3仅拟新增单次3600秒（命令3000/根600）A窗口，待用户具名授权及新ER03/H0/执行/来源/资源/根核验本机Claude PASS后才claim，失败STOP，旧A1/RED/FIX3/FIX19预算与失败不重置。源码轴/I-R/7AC5VER/独立patch和原测试覆盖/超时不改，GREEN、自己ER05及完整C-D均待。六Mega治理归FIX18最终C-D；root Libra三治理仅本地/LTR01承载，不push、不重复用户资格Agent、不等Libra测试窗口。前沿FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3限制不放宽。

> **历史（A2原窗截止前现行摘要）：** **现行 FIX18 A1操作STOP与同窗恢复候选（2026-10-29）：** FIX19已在4986dbac/v0.42.79完整源码/验收/自身Review/签名提交推送/Release及main与tag D闭合；正式Mega27/127、Libra0/26、联合27/153。FIX18唯一RED1已接受（两SHA1负控与两旧converter panic，root633.766/2400秒），产品三文件/九同模块测试和VER1通过不变。A1准入本机Claude5399b119六PASS后，root误将安装清单内二进位SHA当清单文件SHA，自己的前置脚本exit1后仍误发启动；A1仅复制第三方缓存，产品Cargo/fmt/clippy/测试均未开始即受控SIGINT，实际81992/26808f exit1/STOP保留。root159.549秒后检107原库/零新库/owned-live/TMP残留，无手工清理、无观察到Libra升级或二进位漂移。A2仅同窗恢复候选，原A1起06:11:50.1295927Z、命令绝对截止07:01:50.1295927Z、总截止07:11:50.1295927Z不变，含修复/审查耗时；不新增3600秒、不因新目录重置预算。仅在真实本机Claude恢复ER03/GRANULARITY/H0/执行/来源/资源/根核验/VERDICT全PASS且原命令余量≥1500秒时才准一次恢复claim；否则STOP，不自动重启。修正仅仓外操作/守卫：显式绑定四fmt-clippy实际binary及安装清单文件SHA/内嵌binary SHA、每阶段重核、旧partial cache只按已冻清单和owner证明重写；不改产品/旧测试/超时/覆盖。当前A1已claim且STOP、A2/C零claim，GREEN/自己的ER05/完整C-D仍待。六Mega治理归FIX18 C-D，root Libra三治理仅本地签名/LTR01不push；不重复用户资格Agent、不等Libra测试窗口；不复活G13或重置FIX3/FIX19/RED预算。现行前沿 FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL及CL/B3/Import/Blake3范围不放宽；历史FAIL/STOP保留，状态以本摘要及对应卡现行字段为准。

> **历史（A1操作STOP前摘要原文）：** **现行 FIX18 RED已核与A候选（2026-10-29）：** FIX19已在4986dbac/v0.42.79完成源码/A-B/自身Review/签名提交推送/Release及main与tag Docker D，正式Mega27/127、Libra0/26、联合27/153。FIX18三生产文件与九测试候选未变；执行审R1/根R1/R2 FAIL均保留，R3本机Claude `4aa58279-d5ae-4fb9-a899-6f1138143c37` 六PASS/P0/P1=0。唯一RED1实际外层98334/70be51 exit0、测试exit101、恰2目标旧converter panic及2个SHA1负控PASS，compile/list/test退出0/0/101；root99397/8cc25c exit0、633.766/2400秒接受，仅VER1通过。107原DB保留、零新Mega DB/owned-live/TMP残留，无手工清理。A/C尚零claim，GREEN、自己的ER05及完整C/D均待；A1仅候选一次3600秒（命令3000/root600），九新测试/五原TP13/原C段信号/VER5/fmt/strict，实际合同及九治理增量须本机Claude六PASS才可claim。源码/旧测试/原超时覆盖/7AC5VER/三生产文件两目录/独立patch不变；不借或重置FIX3/FIX19/RED预算。六Mega新治理由FIX18完整C/D承载；root Libra三治理仍只本地签名、LTR01承载不push，不重复用户Agent资格任务、不等Libra测试窗口。现行前沿 FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；H0～H6/QUAL/CL直索引债务及B3/Import/Blake3范围不放宽。历史候选、失败及STOP保留，状态以本摘要及对应卡现行字段为准。

> **历史（RED1实际根核验前摘要原文）：** **现行 FIX18 源码候选与首次 RED 边界（2026-10-29）：** FIX19 已在 `4986dbac/v0.42.79` 完成源码/A-B/ER05/C/提交推送/Release/main 与 tag D 闭环，正式完成 Mega27/127、Libra0/26。FIX18 开工本机 Claude R1/R2 FAIL 均保留，R3 `d604149e-81f4-4532-a7b0-1a60be29ee0c` 的 H0/ER03/GRANULARITY/FIX18_ENTRY/VERDICT 均 PASS、P0/P1=0，root 实际收据 `/tmp/fix-mtr18-entry-review-20261029/R3/root-adoption.json`；该 PASS 只准入源码与测试编写。当前三生产文件修复候选与九个同模块测试已保存，生产配置向 C 段索引/补偿器传显式 kind，旧入口兼容、CL 直索引债务及 B3/链/Import/Blake3 限制不变；未宣称 GREEN、自己的 ER05 或完整 C/D。原生 RED 副本基线同为已发布 `4986dbac`，仅追加逐字相同测试模块。首次 `FIX18-RED1` 仅候选一次 2400 秒（命令1800/root600），实际执行合同与本轮九治理状态增量以 `/tmp/fix-mtr18-red1-20261029/execution-review-R3` 本机 Claude 明确终态为准，未 PASS 不 claim；冻结时 RED/A/C 均零 claim。编译、fixture、零匹配、计数、产物来源、资源或预算失败均 STOP，不自动重试；不向 FIX3/FIX19 借预算、不等 Libra 测试窗口。六 Mega 本轮治理继续由 FIX18 完整 C/D 承载；root Libra 三治理在 `439c343b4e8dd92e8e1134cad517dab117e3b368` 精确本地签名之后新增候选仍只归 LTR01，不 push；用户 Agent 的原资格工作不重复，LTR/H1～H6 前驱不放宽。 联合27/153；现行前沿 FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。本页较早候选、失败与STOP保留原时点，状态以本摘要及对应卡现行字段为准。执行前主审R1 `b1414ed0-96d8-496b-ac5a-92c3117e8115` 实际H0/SOURCE_PROOF/RESOURCES PASS、RED_EXECUTION/VERDICT FAIL/P1=1；根核验R1 `fbed0981-d67e-42ae-86d2-5f515cd12a75` 实际ROOT_AUDIT/VERDICT FAIL/P1=3，两份原件均保留。修复对象仅仓外执行/根核验控制器的相对路径、隔离启动和受审绑定，不改产品源码、原测试或2400秒预算；增量R3审查须H0/RED_EXECUTION/SOURCE_PROOF/RESOURCES/ROOT_AUDIT/VERDICT全PASS才可首次claim，当前仍零claim。 R2执行审 `030be07f-efe9-47d8-aa53-2b51bac76523` 实际H0/RED_EXECUTION/SOURCE_PROOF/RESOURCES PASS、ROOT_AUDIT/VERDICT FAIL/P1=1保留；R3仅修字面解释器命令与sys.orig_argv绑定、退出进程采样、根attempt前安全检查和STOP证据保存，2400秒及产品/测试输入不变，实际首次RED仍零claim。

> **历史（执行审R2终态前摘要原文）：** **现行 FIX18 源码候选与首次 RED 边界（2026-10-29）：** FIX19 已在 `4986dbac/v0.42.79` 完成源码/A-B/ER05/C/提交推送/Release/main 与 tag D 闭环，正式完成 Mega27/127、Libra0/26。FIX18 开工本机 Claude R1/R2 FAIL 均保留，R3 `d604149e-81f4-4532-a7b0-1a60be29ee0c` 的 H0/ER03/GRANULARITY/FIX18_ENTRY/VERDICT 均 PASS、P0/P1=0，root 实际收据 `/tmp/fix-mtr18-entry-review-20261029/R3/root-adoption.json`；该 PASS 只准入源码与测试编写。当前三生产文件修复候选与九个同模块测试已保存，生产配置向 C 段索引/补偿器传显式 kind，旧入口兼容、CL 直索引债务及 B3/链/Import/Blake3 限制不变；未宣称 GREEN、自己的 ER05 或完整 C/D。原生 RED 副本基线同为已发布 `4986dbac`，仅追加逐字相同测试模块。首次 `FIX18-RED1` 仅候选一次 2400 秒（命令1800/root600），实际执行合同与本轮九治理状态增量以 `/tmp/fix-mtr18-red1-20261029/execution-review-R2` 本机 Claude 明确终态为准，未 PASS 不 claim；冻结时 RED/A/C 均零 claim。编译、fixture、零匹配、计数、产物来源、资源或预算失败均 STOP，不自动重试；不向 FIX3/FIX19 借预算、不等 Libra 测试窗口。六 Mega 本轮治理继续由 FIX18 完整 C/D 承载；root Libra 三治理在 `439c343b4e8dd92e8e1134cad517dab117e3b368` 精确本地签名之后新增候选仍只归 LTR01，不 push；用户 Agent 的原资格工作不重复，LTR/H1～H6 前驱不放宽。 联合27/153；现行前沿 FIX18→FIX20→FIX21→FIX16→FIX22→FIX17→MTR13→FIX14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。本页较早候选、失败与STOP保留原时点，状态以本摘要及对应卡现行字段为准。执行前主审R1 `b1414ed0-96d8-496b-ac5a-92c3117e8115` 实际H0/SOURCE_PROOF/RESOURCES PASS、RED_EXECUTION/VERDICT FAIL/P1=1；根核验R1 `fbed0981-d67e-42ae-86d2-5f515cd12a75` 实际ROOT_AUDIT/VERDICT FAIL/P1=3，两份原件均保留。修复对象仅仓外执行/根核验控制器的相对路径、隔离启动和受审绑定，不改产品源码、原测试或2400秒预算；合并R2审查须H0/RED_EXECUTION/SOURCE_PROOF/RESOURCES/ROOT_AUDIT/VERDICT全PASS才可首次claim，当前仍零claim。

> **历史（执行审R1前候选摘要原文）：** **现行 FIX18 源码候选与首次 RED 边界（2026-10-29）：** FIX19 已在 `4986dbac/v0.42.79` 完成源码/A-B/ER05/C/提交推送/Release/main 与 tag D 闭环，正式完成 Mega27/127、Libra0/26。FIX18 开工本机 Claude R1/R2 FAIL 均保留，R3 `d604149e-81f4-4532-a7b0-1a60be29ee0c` 的 H0/ER03/GRANULARITY/FIX18_ENTRY/VERDICT 均 PASS、P0/P1=0，root 实际收据 `/tmp/fix-mtr18-entry-review-20261029/R3/root-adoption.json`；该 PASS 只准入源码与测试编写。当前三生产文件修复候选与九个同模块测试已保存，生产配置向 C 段索引/补偿器传显式 kind，旧入口兼容、CL 直索引债务及 B3/链/Import/Blake3 限制不变；未宣称 GREEN、自己的 ER05 或完整 C/D。原生 RED 副本基线同为已发布 `4986dbac`，仅追加逐字相同测试模块。首次 `FIX18-RED1` 仅候选一次 2400 秒（命令1800/root600），实际执行合同与本轮九治理状态增量以 `/tmp/fix-mtr18-red1-20261029/execution-review-R1` 本机 Claude 明确终态为准，未 PASS 不 claim；当前 RED/A/C 均零 claim。编译、fixture、零匹配、计数、产物来源、资源或预算失败均 STOP，不自动重试；不向 FIX3/FIX19 借预算、不等 Libra 测试窗口。六 Mega 本轮治理继续由 FIX18 完整 C/D 承载；root Libra 三治理在 `439c343b4e8dd92e8e1134cad517dab117e3b368` 精确本地签名之后新增候选仍只归 LTR01，不 push；用户 Agent 的原资格工作不重复，LTR/H1～H6 前驱不放宽。

> **历史快照（FIX18源码编写前的现行摘要原文）：** **现行执行前沿（2026-10-29，FIX19 发布后）：** Mega2 main/远端 `4986dbac08d27d11d5b3d1dcf07cb5230e30c976`，签名/DCO 精确九路径提交，签名标注 tag `v0.42.79` 对象 `5f6108860181d5f1101648e2ae8c61e0cee97ed3` 解引用到该提交；手写 Release https://github.com/gitmono-dev/mega2/releases/tag/v0.42.79 已发布。FIX-MTR-19 产品验收与 C/D 已闭合，`done/complete`；现行正式卡数 Mega127/Libra26，完成27/0，联合27/153。下一前沿 FIX-MTR-18 → FIX-MTR-20 → FIX-MTR-21 → FIX-MTR-16 → FIX-MTR-22 → FIX-MTR-17 → MTR13 → FIX-MTR-14 → MTR39 → MTR92 → MTR12/H1；本轮 FIX18 ER-01/02/03 及联合 H0 状态增量待本机 Claude 审查，首次 RED/A/C 均未 claim。Libra 资格工作由用户指定 Agent 独立执行，root 不重复；其最终资格未消费前不放行 LTR10/LTR01，H1～H6 谓词不变。本页较早日期、候选、失败与 STOP 保留其历史含义，以本摘要和对应卡现行字段为准。

**FIX19 真实闭环与继承界限：** C1 3000 秒 STOP、C2 根阶段 STOP 及历次 FAIL 原件保留。C2 七条完整命令实际 exit0，含 nightly fmt、全目标全功能 strict Clippy、加载 `.env.test` 的完整 `cargo test --all`（22 目标、3178 ok、0 failed、3 既有 ignored）、双 build、metadata、真实 Redis/Git 服务证明；命令层 2117.636 秒。用户“授权你可以先闭合 FIX19”承接精确 21 库及一次 C3 1800 秒根收口授权，累计 ER-10 cap 9600→11400 秒；C3 在授权后 60 分钟内启动，runner 实际 exit0/44.105 秒，21 条受控 DROP、原107库保留；root 于 2026-10-29T01:43:20Z 独立签收本地 C。受测完整树 SHA256 `8ad13bdd23196908a564122b3111ecbb675e53038162e141bb052a9c7140cd75` 至 D 未变。最终本机 Claude 发布审 R1 `af930d9d-6215-47fc-9056-94a8518f6530` ER05/COVERAGE PASS、RELEASE/VERDICT FAIL（发布说明缺栏目）；仓外说明修正后 R2 `68536851-0b02-4a7f-aa3f-ed138ad92ce1` P1_CLOSURE/RELEASE/VERDICT PASS、P0/P1=0。main Harbor run 40803575235 于 02:09:53Z success；tag Docker run 40803628462 于 02:14:25Z success（amd64、arm64、docker 三 job 全成功），实读镜像清单 digest `sha256:0610428ce4db5ea0344b221715dc8bbf78f6de694dc3fe68134158efc055931a` 含两架构。原件 `/tmp/fix-mtr19-publication-20261029/{C-receipt,D-receipt,main-D,tag-D,docker-manifest}.json`、`tag-D.log` 与 `/tmp/fix-mtr19-C3-root-only-20261029/root-acceptance.json`；这些事实不证明历史 FIX3 数据库根因，不重置其 3 诊断/8 A-B STOP。

**后置治理承载与 FIX18 当前阻塞：** FIX19发布与D已闭合；FIX18 RED1/A3及自身ER05 R2 PASS保留，C1全量exit101/STOP、21新库受审清理且root核原107库与五服务；D1的39/39 stdout因NULL观测器STOP不计诊断通过。D2实际本机Claude R5六PASS/P0P1零、受控整组SSH39/39及独立根核PASS（102.280/900秒），不证明C1历史根因或完整C。当前`blocked/locally-accepted`，C2五门命令exit0但根R1 STOP、25库清理已核，C3完整合同待审零claim，完整C/提交/推送/tag/Release/D未取得。六Mega治理仍随FIX18最终C/D，Libra三治理只本地由LTR01承载；`DEBT-TEMPLATE-MAIN-D`仍由root按独立ER03处理，H0～H6及后继边界不扩大。
**历史（C2执行前后置治理段）：** **后置治理承载与 FIX18 当前阻塞：** FIX19发布与D已闭合；FIX18 RED1/A3及自身ER05 R2 PASS保留，C1全量exit101/STOP、21新库受审清理且root核原107库与五服务；D1的39/39 stdout因NULL观测器STOP不计诊断通过。D2实际本机Claude R5六PASS/P0P1零、受控整组SSH39/39及独立根核PASS（102.280/900秒），不证明C1历史根因或完整C。当前`blocked/locally-accepted`，C2完整合同待审且零claim，完整C/提交/推送/tag/Release/D未取得。六Mega治理仍随FIX18最终C/D，Libra三治理只本地由LTR01承载；`DEBT-TEMPLATE-MAIN-D`仍由root按独立ER03处理，H0～H6及后继边界不扩大。
> **历史（D2根核前后置治理段）：** **后置治理承载与 FIX18 当前阻塞：** FIX19发布与D已闭合；FIX18 RED1/A3及自身ER05 R2本机Claude PASS保留，C1全量exit101/STOP，当前`blocked/locally-accepted`。受审C1精确清理已仅删除新21库并由root核原107库及五服务保持；整组SSH诊断D1测试进程39/39通过，但NULL观测器使D1控制器exit1/STOP、未获诊断验收。D2单次900秒恢复待最终本机Claude审查，C2未claim，完整C、提交、推送、tag、Release与D均未取得。六Mega治理仍由FIX18最终C/D承载，Libra三治理只本地由LTR01承载；`DEBT-TEMPLATE-MAIN-D`仍由root按独立ER03处理；H0～H6及后继边界不扩大。
> **历史（D1 STOP前后置治理段）：** **后置治理承载与 FIX18 当前阻塞：** FIX19的发布与D已闭合；FIX18 A3根核验及自身ER05 R2本机Claude九PASS/P0/P1=0保留，C1完整测试exit101/STOP，当前`blocked/locally-accepted`，0.42.80完整C与实际main/tag D均未取得。21个C1新增测试库的精确清理、一次诊断和C2仅候选待本机Claude审查，六Mega治理仍由FIX18最终C/D承载，root Libra三治理只本地由LTR01承载。模板旧main-D快照债务`DEBT-TEMPLATE-MAIN-D`按独立ER03处理；H0～H6和后继边界不扩大。
> **历史（C1 STOP前后置治理段）：** **后置治理承载与 FIX18 开工边界：** FIX19的发布与D已闭合；FIX18 A3根核验通过、自身ER05 R2本机Claude九PASS/P0/P1=0，当前in-progress/locally-accepted，0.42.80版本化完整C与实际main/tag D待。A1 STOP、A2零claim过期、自身ER05 R1 FAIL原件保留；六Mega治理由FIX18最终C/D承载，root Libra三治理继续只本地由LTR01承载。模板旧main-D快照债务`DEBT-TEMPLATE-MAIN-D`仍按独立ER03处理，H0～H6及后继边界不扩大。
> **历史（自身ER05 R2前后置治理段）：** **后置治理承载与 FIX18 开工边界：** FIX19已完成发布与D；FIX18的RED1已接受、A1操作STOP及A2过期零claim原件保留，A3经用户单次授权、计划R6和执行审R3本机Claude PASS后已于08:43:45Z claim，9新/5原TP13/1原信号、VER5、nightly fmt、严格Clippy与独立根核验均通过（648.364/3600秒，收据`/tmp/fix-mtr18-A3-20261029/root-acceptance.json`）。本卡仍须自己的ER05、版本化完整C和main/tag实际D；六Mega治理由本卡最终C/D承载，root Libra三文档继续只本地由LTR01承载。模板旧main-D快照债务`DEBT-TEMPLATE-MAIN-D`仍按独立ER03处理，H0～H6与后继边界不扩大。
> **历史（A3根核验前后置治理段）：** **后置治理承载与 FIX18 开工边界：** FIX19 D 后才写入本轮六 Mega 治理，归 FIX18 已列 I 并由其最终完整 C/D 承载；root Libra 独立治理副本三文档继续只本地签名，由 LTR01 唯一发布，不扩大 push 权限。若 FIX18 无法开工，root 保留本轮字节和原件，按 ER03 另登记独立仅证据治理承载卡并审查，不借未验收源码推送。FIX18 保留公开旧入口，新增生产实际消费的显式 kind，三生产文件/两目录、7AC/5VER、独立 patch；CL/Review 直索引债务仍待其具名门重审。RED/A/C 各自冻结合同与本机 Claude PASS 后方可单次执行；RED1已按原窗执行并接受，A1已claim且STOP，A2同原窗恢复审PASS但过期零claim，A3新一次A窗口已获用户具名授权，最终执行审待且A3零claim，C仍零claim；不等待 Libra 测试窗口。模板旧 main-D 快照单独具名为 `DEBT-TEMPLATE-MAIN-D`（owner=root；任何模板编辑前须先登记独立ER03卡并审查，本轮不授该编辑权限；FIX19 D 后另卡同步），实际 D 始终按 live workflow，不在 FIX18 越界修改模板。

> **历史（A3授权前后置治理段）：** **后置治理承载与 FIX18 开工边界：** FIX19 D 后才写入本轮六 Mega 治理，归 FIX18 已列 I 并由其最终完整 C/D 承载；root Libra 独立治理副本三文档继续只本地签名，由 LTR01 唯一发布，不扩大 push 权限。若 FIX18 无法开工，root 保留本轮字节和原件，按 ER03 另登记独立仅证据治理承载卡并审查，不借未验收源码推送。FIX18 保留公开旧入口，新增生产实际消费的显式 kind，三生产文件/两目录、7AC/5VER、独立 patch；CL/Review 直索引债务仍待其具名门重审。RED/A/C 各自冻结合同与本机 Claude PASS 后方可单次执行；RED1已按原窗执行并接受，A1已claim且STOP，A2同原窗恢复审PASS但过期零claim，A3新一次A窗口仅候选待用户具名授权与执行审，C仍零claim；不等待 Libra 测试窗口。模板旧 main-D 快照单独具名为 `DEBT-TEMPLATE-MAIN-D`（owner=root；任何模板编辑前须先登记独立ER03卡并审查，本轮不授该编辑权限；FIX19 D 后另卡同步），实际 D 始终按 live workflow，不在 FIX18 越界修改模板。

> **历史（FIX18 RED1/A1执行前原文）：** **后置治理承载与 FIX18 开工边界：** FIX19 D 后才写入本轮六 Mega 治理，归 FIX18 已列 I 并由其最终完整 C/D 承载；root Libra 独立治理副本三文档继续只本地签名，由 LTR01 唯一发布，不扩大 push 权限。若 FIX18 无法开工，root 保留本轮字节和原件，按 ER03 另登记独立仅证据治理承载卡并审查，不借未验收源码推送。FIX18 保留公开旧入口，新增生产实际消费的显式 kind，三生产文件/两目录、7AC/5VER、独立 patch；CL/Review 直索引债务仍待其具名门重审。RED/A/C 各自冻结合同与本机 Claude PASS 后方可单次执行，当前零 claim；不等待 Libra 测试窗口。模板旧 main-D 快照单独具名为 `DEBT-TEMPLATE-MAIN-D`（owner=root；任何模板编辑前须先登记独立ER03卡并审查，本轮不授该编辑权限；FIX19 D 后另卡同步），实际 D 始终按 live workflow，不在 FIX18 越界修改模板。

> **历史快照（FIX19 发布前原文）：** **现行联合 H0 恢复边界（2026-10-29，待本轮审查）：** Mega2 main `3abc2558071fbd4306d1df0d983fe26d31f29b19` 已发布，FIX-MTR-19 的 `0.42.79` 仅本地候选；C1 因 3000 秒命令上限 STOP，授权精确清理的 21 个零连接测试库已删除且原 107 库逐项保持，C2 仅一次 6000 秒（完整命令 5400 秒、root 核验 600 秒）尚未 claim。清理后本地 Claude H0 R9 FAIL/P1=3、R10 FAIL/P1=1、R11 PASS、R12 PASS 均保留且后两次只覆盖各自冻结字节。Libra 独立治理副本已把本地 `3fa8f11` 与上游 `d0a5c28` 原生双父合并为本地签名/DCO `1c57dc877c079119ac804532ee533ec02df9bb64`，19 路径核对、16 个上游非冲突文件逐字节保留；合并树 Claude R1 FAIL/P1=1、R2 `e2037c44-6fd4-4cc7-9924-db266c128ffb` 四 PASS/P0=P1=0，R2 六项 P2 的处置在本轮继续；联合 H0 R13 `a2628cc3-cd83-45dd-baee-a0d080a16e44` 实际 H0/VERDICT FAIL、EVIDENCE PASS、P0=0/P1=1，三处 Mega 现行字段仍写“待R12审查”；该失败保留；R14 `0a3cb2dd-9920-4798-8f7a-5ebb553e9c80` 实际 H0/EVIDENCE/VERDICT 三 PASS、P0/P1=0，只覆盖本轮 P2 修订前的冻结字节；本次增量由 R15 审查，未取得明确 PASS 前不 claim C2。本地合并签名收据未独立记录校验脚本/树哈希且其“未推送”布尔值未核远端，LTR01 或任何推送前须重核远端 ref、签名、写集。合并尚未推送，Libra `DEP-TR-QUAL-REPAIRS` 仍 incoming（F2 unknown/P1、FIX-LTR-06 仅登记、最终资格和原件缺失）；Mega FIX19 C2 执行审/完整 C/D、Libra LTR10/H1/LTR01 均未获放行。两仓当前九治理字节及唯一 Mega `docs/refactoring/traces.md` 待本轮本地 Claude 联合 H0 PASS；发布承载仍为 Mega FIX19 自身独立 patch C/D 与 Libra LTR01，不提前 push。

> **历史快照（本轮联合 H0 修订前原文）：** **现行清理后审查边界（2026-10-28）：** 本地Claude H0 R9 `fe3f75d5-5266-44c6-b012-85789901b715` 实际H0/EVIDENCE/VERDICT FAIL，P0=0/P1=3、输入无漂移；缺大文件原始差异、MTR-01字段标签移位、FIX19两段旧C候选未标历史，已前滚纠正并待增量审查。R8七PASS仅覆盖精确清理前冻结输入；后续本机Claude H0 R10 `fb8f44e0-f88d-47d6-8eb3-8619b4372cf6` 实际H0/VERDICT FAIL、EVIDENCE PASS、P0=0/P1=1、无输入漂移；该P1为H0证据单元误记R4结果，已纠正待复审；R11本地Claude `9cf9a637-40da-41ac-b77f-e8cee6eb3e4c` H0/EVIDENCE/VERDICT PASS、P0/P1=0，仅覆盖本轮P2载体对齐前冻结字节；当前新增对齐待R12增量审查；FIX19 C1 STOP、0.42.79未发布；C2尚未claim，FIX19与后继仍阻塞。

> **历史（FIX19发布前原文）：** **现行 FIX19-C1 精确清理已完成（2026-10-28）：** 本机 Claude R8 session `07f3e532-522e-4d70-ba4f-9a6f503d2d3e`，七项RECOVERY/RESOURCE/H0/CLEANUP/SOURCE/AUTHORIZATION/VERDICT均PASS，P0/P1=0；131冻结输入及模式无漂移。root按授权一次清理C1新增的21个零连接测试库，21条DROP exit0，清理终态PASS，现场与原C1前107库清单逐项相等；此事不证明历史C2数据库根因或FIX14代码完成。FIX-MTR-19 C1仍STOP且blocked/locally-accepted，0.42.79仅本地候选；用户授权C2总上限180分钟，当前仅一次6000秒（命令5400/root600）候选尚未claim、执行审查/完整C/D待，后继FIX18仍阻塞。未提交、推送、tag、Release或CI/D；Mega测试无需等待Libra测试窗口。R1–R7原PASS/FAIL与C1预算均保留。

> **历史快照（R8前顶部原文）：** **现行 FIX19-C1 清理专项 R7 审查失败（2026-10-28）：** 本机 Claude session `682f4a69-c9e7-4e0e-866f-41815f55c344`，manifest `e5b765bfd7db0e6daf357c94936b10b126962d83edce706c46242477a8b677ec`、output `f91edac23ffb9655b332c2aaa42c90c2c637f41692d374593a6f7cc74cda4829`，RECOVERY/RESOURCE/CLEANUP/SOURCE/AUTHORIZATION PASS、H0/VERDICT FAIL，P0=0/P1=1；三处授权状态旧文已前滚修正，R7原FAIL保留，待R8复审。用户授权21库精确清理及C2上限180分钟；当前较窄候选仅一次6000秒（命令5400/root600），测试自身timeout不变。FIX-MTR-19 C1已实际STOP，仍blocked/locally-accepted；21库尚未清理，最终审查、C2完整C/D、提交、推送、tag、Release、CI/D均未完成；0.42.79仅本地。Mega测试无需等待Libra测试窗口。

> **历史快照（R7前顶部原文）：** **现行 FIX19-C2 恢复 R6 H0审查失败、用户已授权（2026-10-28）：** 本机 Claude R6 session `057d5e0e-d244-496f-be3b-cd25c86f25c9`，RECOVERY/RESOURCE PASS、H0/VERDICT FAIL，P0=0/P1=1，manifest `42448e95e9b143bbbd1ee6905d9efb8021420d11defe67c4cc83571d72d64291`；Mega H0证据列旧状态已前滚修正，待下一次实际审查。用户对含21库精确清理和6000秒C2的提问答复“授權精準清理及 180 分鐘超時的 C2”；按较窄一次6000秒（命令5400/root600）候选执行，180分钟为授权上限，测试自身timeout不变。FIX-MTR-19 C1已实际STOP，仍blocked/locally-accepted；21库尚未清理，最终清理/恢复H0/C2执行审查及C验收待，0.42.79仅本地候选，无提交、推送、tag、Release、CI/D。Mega测试无需等待Libra测试窗口；旧R1–R6 FAIL、R4条件PASS与C1预算原样保留。

> **历史快照（R6前顶部原文）：** **现行 FIX19-C2 恢复 R5 H0审查失败（2026-10-28）：** 本机 Claude session `18b1bba6-cbde-4053-ab86-134ccb9f16d1`，manifest `98a56f621deacec0d04d51850ae9f95fd3877f2cb7c20c46e613a6b572ef6bfb`、output `23c91e9acd0e46acc675e1a4f4218e0d719d64098d3465b101670b021b522455`，RECOVERY/RESOURCE PASS、H0/VERDICT FAIL，P0=0/P1=1；M0与Libra H1两个旧现行单元已前滚修正，尚待复审。先前 R4 session `43c72d29-4c7e-49a1-bae7-f7f1b222480e` 四PASS只覆盖旧字节。FIX-MTR-19 C1已实际STOP，仍 blocked/locally-accepted；21库未清理，FIX19-C2单次6000秒（命令5400/root600）及精确清理待用户明确授权和最终审查，尚未claim。0.42.79仅本地候选；完整C、提交、推送、tag、Release、CI/D未完成。Mega测试无需等待Libra测试窗口；历史R1/R2/R3失败、C1 STOP与预算保留。

> **历史快照（R5前顶部原文）：** **现行 FIX19-C2 条件恢复审 R4 PASS（2026-10-28）：** 本机 Claude session `43c72d29-4c7e-49a1-bae7-f7f1b222480e`，manifest `2e5c08efca448850047161ce5aa79253ee7ab11c313216ec2763ded12853efa0`、output `fd96004671d39d0da40e9aa0a16d5f368b4d6dde43872293f82d0b6bcd8d9bda`，RECOVERY/RESOURCE/H0/VERDICT PASS、P0/P1=0；12项P2已在仓外恢复合同逐项处置，改后清理脚本仍须最终专审。FIX-MTR-19 C1已实际 STOP，仍为 blocked/locally-accepted；21个测试库未清理，FIX19-C2单次6000秒（命令5400/root600）及精确清理仍待用户明确授权、最终H0与执行审查，尚未claim。0.42.79仅本地候选；源码A/B已保存，完整C、提交、推送、tag、Release、CI/D均未完成；Mega测试无需等待Libra测试窗口。R3及更早失败、C1预算保持历史。

> **历史快照（R4前顶部原文）：** **现行 FIX19-C2 恢复 R3 审查失败（2026-10-28）：** 恢复计划本机Claude R3 session `6358fac8-88af-4897-a0d2-fc04efa683c7`，manifest `0ebb2eaf5bda493559a8953bf9f8f072a0a444fe039f6c33b61f26517f3585ff`、output `551c9b44df62e5f8284286f2ef1b6744884b7d14eb81c4a3b2b91264442926d3`，RECOVERY PASS、RESOURCE/H0/VERDICT FAIL，P0=0/P1=2；C2 6000秒预算已获审查认可，失败项为14个现行旧时长单元及清理授权绑定薄弱，均已前滚修正待新审。R1/R2 FAIL与C1 STOP原件保留。 FIX19仍blocked/locally-accepted；21库未清理，FIX19-C2单次6000秒未获用户答复/未claim，未提交/推送/tag/Release/D。 当前版本面0.42.79仍只在本地候选，未发布。 H0本机Claude session `65a9eab9-9d84-48c8-ade0-e84f4d2c5ca2`，manifest `98751525b594fb5cdce99a498b4a5c477779ad03f25d09e0652609b4bc426dbc`、output `1b212da1fa74f59072688624e117138abc7a682310a4cf653aa3d9700c03335f` 实际PASS；Libra治理本地signed/DCO提交 `10acbeb84fd5b2bf598fcbd5cb3d60c1649cad97` 未推送。C1执行审R1 session `357d4500-95b9-4635-8f1a-da4237115c25` FAIL（manifest `b45a1d7bede6bcb2ce07d3a693dcc3cc002c5f0795b63a2570b776fe46427461`、output `0a8af3019147df9500a1c3206363d9ec0acc6fd72d93e848e4e3547d411d6259`）；R2 session `7edfef1f-3f0c-4044-94dd-1fe37323fa0f` FAIL（manifest `4fbd732290268044c9deff27883ea3e266f0dd2dd5b0c3e9f8949594575f4b74`、output `e457fc1fd35004907d6f458d8a84d80e5d191ccc8f9a57a643243e1d888d765b`）；R3 session `7beb85af-da4b-485f-a003-0602296ecfb7` PASS仅覆盖旧字节（manifest `6a782b96f4778a37da582a2edd32fa23e69730d1c5c7ee887ad234704523f7ce`、output `4653dc846d19692c958749989074f2a2948425d2e0d4ea31d1532d2b73aedd41`）；R4 session `7effd3eb-4f8b-4460-9df7-1b1746d87af7` PASS仅准C1执行（manifest `b89e84eaff4c72919c7b0e6c32f8c420cbf0fa9f898f973db60eab2c51a5222b`、output `8403ee50effc159da79a5baf414e886b16fe95e1c443080a9433198048cfbe44`）。 恢复计划R1 session `fa3bc066-44a9-4d57-84df-8c6561735235`，manifest `88a3669702d432d3bebabfd11f32041a7174d6f9e8b40146c3c320670d2a862e`、output `11c93967df9bc5c2d2be2d5a66234fe949e9387940352e350495e417cae7796f`，RECOVERY/H0/VERDICT FAIL、RESOURCE PASS、P0=0/P1=4；R2 session `ab766135-f857-48d2-9d4c-13228778880a`，manifest `18fb4cd62a2c104467ffb6888cf74c13fa551b03dc231dedc201c45fb0f4f6c7`、output `b35950f59b779ab48c6b5a39212eedae4de601f18a38485796c661e8d48b80af`，RECOVERY/RESOURCE/H0/VERDICT FAIL、P0=0/P1=4；均为历史失败。

> 历史（R3前顶部原文）：**现行 FIX19-C2 恢复 R2 审查失败（2026-10-28）：** FIX-MTR-19 C1实际STOP且0.42.79未发布，14目标摘要/3086 ok/0 FAILED仅部分观察、21测试库保留；FIX19 blocked/locally-accepted，FIX19-C2一次6000秒（命令5400/root600）及精确清理待用户授权和新审，FIX18与LTR后继不放行。 恢复计划R1 session `fa3bc066-44a9-4d57-84df-8c6561735235`，manifest `88a3669702d432d3bebabfd11f32041a7174d6f9e8b40146c3c320670d2a862e`、output `11c93967df9bc5c2d2be2d5a66234fe949e9387940352e350495e417cae7796f`，RECOVERY/H0/VERDICT FAIL、RESOURCE PASS、P0=0/P1=4；R2 session `ab766135-f857-48d2-9d4c-13228778880a`，manifest `18fb4cd62a2c104467ffb6888cf74c13fa551b03dc231dedc201c45fb0f4f6c7`、output `b35950f59b779ab48c6b5a39212eedae4de601f18a38485796c661e8d48b80af`，RECOVERY/RESOURCE/H0/VERDICT FAIL、P0=0/P1=4；均为历史失败，现正前滚修正。 Mega/Libra并行测试按用户指令继续，但并行负载的C预算必须单独估算；原Libra仓库由用户指定Claude独占。 H0本机Claude session `65a9eab9-9d84-48c8-ade0-e84f4d2c5ca2`，manifest `98751525b594fb5cdce99a498b4a5c477779ad03f25d09e0652609b4bc426dbc`、output `1b212da1fa74f59072688624e117138abc7a682310a4cf653aa3d9700c03335f` 实际PASS；Libra治理本地signed/DCO提交 `10acbeb84fd5b2bf598fcbd5cb3d60c1649cad97` 未推送。C1执行审R1 session `357d4500-95b9-4635-8f1a-da4237115c25` FAIL（manifest `b45a1d7bede6bcb2ce07d3a693dcc3cc002c5f0795b63a2570b776fe46427461`、output `0a8af3019147df9500a1c3206363d9ec0acc6fd72d93e848e4e3547d411d6259`）；R2 session `7edfef1f-3f0c-4044-94dd-1fe37323fa0f` FAIL（manifest `4fbd732290268044c9deff27883ea3e266f0dd2dd5b0c3e9f8949594575f4b74`、output `e457fc1fd35004907d6f458d8a84d80e5d191ccc8f9a57a643243e1d888d765b`）；R3 session `7beb85af-da4b-485f-a003-0602296ecfb7` PASS仅覆盖旧字节（manifest `6a782b96f4778a37da582a2edd32fa23e69730d1c5c7ee887ad234704523f7ce`、output `4653dc846d19692c958749989074f2a2948425d2e0d4ea31d1532d2b73aedd41`）；R4 session `7effd3eb-4f8b-4460-9df7-1b1746d87af7` PASS仅准C1执行（manifest `b89e84eaff4c72919c7b0e6c32f8c420cbf0fa9f898f973db60eab2c51a5222b`、output `8403ee50effc159da79a5baf414e886b16fe95e1c443080a9433198048cfbe44`）。

> 历史（R2前顶部原文）：**现行 FIX-MTR-19 C1 STOP 后恢复 R1 审查失败（2026-10-28）：** FIX-MTR-19 C1已claim并于2026-10-28 13:16:58 UTC因3000秒上限STOP，0.42.79未发布；metadata/fmt/strict Clippy PASS，全量仅14目标完成摘要、3086 ok/0 FAILED为部分观察，build/build-tests/service-proof/root C未运行，21个新零连接测试库保留；精确清理和FIX19-C2新窗口均待用户授权及新审，后继FIX18阻塞。 H0本机Claude session `65a9eab9-9d84-48c8-ade0-e84f4d2c5ca2`，manifest `98751525b594fb5cdce99a498b4a5c477779ad03f25d09e0652609b4bc426dbc`、output `1b212da1fa74f59072688624e117138abc7a682310a4cf653aa3d9700c03335f` 实际PASS；Libra治理本地signed/DCO提交 `10acbeb84fd5b2bf598fcbd5cb3d60c1649cad97` 未推送。C1执行审R1 session `357d4500-95b9-4635-8f1a-da4237115c25` FAIL（manifest `b45a1d7bede6bcb2ce07d3a693dcc3cc002c5f0795b63a2570b776fe46427461`、output `0a8af3019147df9500a1c3206363d9ec0acc6fd72d93e848e4e3547d411d6259`）；R2 session `7edfef1f-3f0c-4044-94dd-1fe37323fa0f` FAIL（manifest `4fbd732290268044c9deff27883ea3e266f0dd2dd5b0c3e9f8949594575f4b74`、output `e457fc1fd35004907d6f458d8a84d80e5d191ccc8f9a57a643243e1d888d765b`）；R3 session `7beb85af-da4b-485f-a003-0602296ecfb7` PASS仅覆盖旧字节（manifest `6a782b96f4778a37da582a2edd32fa23e69730d1c5c7ee887ad234704523f7ce`、output `4653dc846d19692c958749989074f2a2948425d2e0d4ea31d1532d2b73aedd41`）；R4 session `7effd3eb-4f8b-4460-9df7-1b1746d87af7` PASS仅准C1执行（manifest `b89e84eaff4c72919c7b0e6c32f8c420cbf0fa9f898f973db60eab2c51a5222b`、output `8403ee50effc159da79a5baf414e886b16fe95e1c443080a9433198048cfbe44`）。恢复计划本机Claude R1 fa3bc066 的 RESOURCE PASS、RECOVERY/H0/VERDICT FAIL，P0=0/P1=4；四项为C2时限依据不足、现行索引滞后、Libra H0未前滚、超预算Lifecycle未blocked。本次正逐项纠正，R1原件和C1 STOP保留。Mega与Libra目标目录分离，按用户新指令Mega验收不等待Libra测试窗口；不将并行解释为新C窗口或清理授权。正式完成数Mega26/Libra0不变，未提交Mega代码、未推送/Tag/Release/D。

> 历史（恢复R1纠正前顶部原文）：**现行 FIX-MTR-19 C1 STOP（2026-10-28 13:16:58 UTC）：** 用户明确允许 Mega2 与 Libra 在独立目标目录并行测试，不再等待 Libra 测试窗口。本地 Claude 最终 C1 执行准入 R4 四项 PASS、P0/P1=0，仅批准一次执行合同。C1 实际 claim 后将版本面改为 0.42.79；metadata、nightly fmt、严格 Clippy 均 exit 0。`source .env.test && cargo test --all` 在 3000 秒命令上限耗尽时尚未结束，执行器按合同 SIGTERM 收束，外层 exit 1/STOP；仅 14 个目标有完成摘要、观察到 3086 个 ok 和 0 个 FAILED，不构成全量 PASS。build、build --tests、额外 Redis/Git 证明、root C 验收、精确提交、main/tag 推送、Release、远端 D 均未执行。C1 原件 `/tmp/fix-mtr19-C1-candidate-20261028`，终态 SHA256 `84bc329f51f3ec57c90e1c1c4180f67bc56c41e957e7a8b49b6d519fd17c5783`；只读诊断 `/tmp/fix-mtr19-C1-recovery-20261028/postmortem.json`。原 107 个数据库未变，新增 21 个零连接测试数据库（18 个 mega2_test、3 个 mega2_wh03）暂保留；服务、容器和追踪目录一致，观察到的子进程已闭合。C1 预算和一次 claim 已消费，不自动重试、不回滚 0.42.79。FIX19 仍 in-progress/locally-accepted，FIX18 与后继仍阻塞，正式完成数不变。下一步先按 ER-03/ER-10 审查具名资源清理及 C2 恢复合同；新窗口和数据库清理待用户授权，不放宽全量 C、构建、ER-05 或版本 tag Docker D。Libra 原目录继续由用户指定 Claude 独占。

> 历史（C1 STOP 前顶部原文）：**现行FIX19治理复审失败及修正（2026-10-28）：** 本机Claude 4b5ae973于2026-10-28T08:00:18Z真实ER03 PASS、H0_METADATA/VERDICT FAIL、outer1/chunk10802d；此前四类P1已关闭，新三P1为Libra两现行恢复段、Libra H1和Mega执行总表状态陈旧。已前滚并保留原文，必须真实复审，未提交、未C/D；前次所有FAIL/STOP不变。 Libra资格R5/R6已PASS但本地门、FIX-LTR-06/F2与最终资格仍待，历史缺失原件/公钥不可补造；root已把共享测试窗口明确交予用户指定的Libra Claude，待其归还前root不启动Cargo/测试。 5b411f0a实际H0_METADATA/VERDICT FAIL，六个H0证据单元已前滚并待复审；旧FAIL和STOP保留。

> 历史（4b5ae973纠错前顶部原文）：**现行FIX19治理纠错（2026-10-28）：** 现行127/26（Mega127/Libra26），完成26/0、联合26/153；MTR40→FIX-MTR-15→FIX-MTR-19→FIX-MTR-18→FIX-MTR-20→FIX-MTR-21→FIX-MTR-16→FIX-MTR-22→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。计划R3已准入；FIX15已实际发布3abc2558/v0.42.78并完成C/D。FIX19源码、受控RED、A3实际8/8与完整15/15（0ignored）、fmt/strict/VER4及root1193.712/1800秒资源闭合已核；产品ER05 R1/R2 PASS。R2的H0/总FAIL及单次H0恢复507630b4的H0/总FAIL均保留并STOP，当前是同I内确定性治理纠错待复审；未提交、未C/发布，正式done不增加。MTR13仍blocked，FIX18及后继不得越过FIX19完整C/D。 当前只按ER03/ER10在既有九治理I内确定性纠错；依据ER05修复后复审直到明确PASS，每次本机Claude调用最多900秒，失败保留且停止该调用，修正前不重试；原两次自身循环和单次H0恢复STOP保持。此处没有新增/重置产品A/C、FIX3三诊断八A-B或C草稿预算，不复活G13、不免版本tag Docker D。当前治理审通过后仅允许三Libra文档精确本地提交；完整C另有最终执行审及真实60分钟资源分配，六Mega治理由FIX19最终C/D承载。

> 历史（确定性治理纠错前顶部原文）：**现行FIX19 H0文字恢复（2026-10-28）：** 自己的ER05 R1三PASS后，增量R2实际ER05 PASS、H0/VERDICT FAIL（两处现行字段陈旧）；本轮两次审查已耗尽并STOP，不能称最终治理已通过。源码/8新增测试、受控RED、A3实际8/15/fmt/strict/VER4及资源闭合不变，正式完成数仍26/0。现仅按ER03/ER10前滚H0文字：纠正Libra DEP-TR-H1、Mega追溯行，以及MTR01 Current evidence/执行总表两项附带记账；扩展逐列扫描。root登记一次独立H0恢复审（最多900秒、一次，真实本机Claude），仅审本次治理/P1关闭及完整九文件差异，不重跑或重置自己的ER05循环、产品A/C或FIX3预算，不作第三次C草稿审。H0恢复实际PASS及最终C准入前不提交、不bump、不push；三Libra治理仍仅本地signed、LTR01承载，原Libra由用户Claude独占。

> 历史（H0恢复前顶部原文）：**现行FIX19 A3验收收据（2026-10-28）：** 源码与8新增测试已保存；A3一次真实执行8/8、签名完整模块15/15（0ignored、两条Git真实验签）、nightly fmt、strict Clippy及VER4全部exit0。命令657.744秒，root1193.712/1800秒内资源闭合，107原数据库与原资源保留、无手工清理。首次root后检因新PackageKit进程exit1，待其自然退出后仅重核资源一次通过，未重跑产品命令或扩大原窗口；原FAIL/STOP均保留。FIX19现in-progress/locally-accepted，后置治理B及自己的ER05 R1已过；R2失败后的H0文字恢复审与完整C/D待；未提交、push、bump、tag或Release。正式127/26、完成26/0不变。六Mega治理由FIX19承载，三root Libra治理只精确本地签名、LTR01承载且不push；原Libra由用户指定Claude独占交接。A3资源窗口已关闭，不自动授予Libra或C窗口。以下带日期快照保留原事实。  **历史（H0恢复前，原文）：** **现行FIX19 A3验收收据（2026-10-28）：** 源码与8新增测试已保存；A3一次真实执行8/8、签名完整模块15/15（0ignored、两条Git真实验签）、nightly fmt、strict Clippy及VER4全部exit0。命令657.744秒，root1193.712/1800秒内资源闭合，107原数据库与原资源保留、无手工清理。首次root后检因新PackageKit进程exit1，待其自然退出后仅重核资源一次通过，未重跑产品命令或扩大原窗口；原FAIL/STOP均保留。FIX19现in-progress/locally-accepted，后置治理B及自己的ER05 R1已过；新增记账/完整差异增量审与完整C/D待；未提交、push、bump、tag或Release。正式127/26、完成26/0不变。六Mega治理由FIX19承载，三root Libra治理只精确本地签名、LTR01承载且不push；原Libra由用户指定Claude独占交接。A3资源窗口已关闭，不自动授予Libra或C窗口。以下带日期快照保留原事实。 **历史（自己的ER05 R1后记账前）：** **现行FIX19 A3验收收据（2026-10-28）：** 源码与8新增测试已保存；A3一次真实执行8/8、签名完整模块15/15（0ignored、两条Git真实验签）、nightly fmt、strict Clippy及VER4全部exit0。命令657.744秒，root1193.712/1800秒内资源闭合，107原数据库与原资源保留、无手工清理。首次root后检因新PackageKit进程exit1，待其自然退出后仅重核资源一次通过，未重跑产品命令或扩大原窗口；原FAIL/STOP均保留。FIX19现in-progress/locally-accepted，后置治理B已过；自己的ER05及完整C/D待；未提交、push、bump、tag或Release。正式127/26、完成26/0不变。六Mega治理由FIX19承载，三root Libra治理只精确本地签名、LTR01承载且不push；原Libra由用户指定Claude独占交接。A3资源窗口已关闭，不自动授予Libra或C窗口。以下带日期快照保留原事实。

> 历史A3入场前快照（原文）：**现行FIX19执行器恢复（2026-10-27）：** A1错源产物与A2启动校验均实际FAIL/STOP，预算分别已消费；A2在seed-cache exit1后停止，未复制缓存、未Cargo、未产品测试，独立target仍不存在。root257.053/1800秒内核107原库及资源不变、无手工清理；后检还发现非运行时Claude自动更新，不以独立资源检查冒充A通过。FIX19源码候选/受控RED保留，blocked/空。A3仅修外部父/子环境校验及审查工具与运行时工具分离；一次1800秒（命令1500/root300）新窗口候选，实际Claude ER03/H0/执行/资源PASS后才可claim，自己的ER05及完整C/D仍待。正式127/26、完成26/0不变；六Mega治理由FIX19 C/D、三root Libra治理仅LTR01承载且不push。用户已指定Claude接续原Libra资格，须先接管Grok写集；root不重复，Cargo/发布继续跨仓互斥。以下带日期快照保留原事实。

> 历史A2入场前快照（原文）：**现行FIX19产物来源恢复（2026-10-27）：** A1已实际FAIL/STOP：原命令exit101、0通过/8失败/0ignored，后续完整15/fmt/strict/audit未跑；root90.653/1800秒内核107原库与资源保留、无手工清理。执行二进制sha7b9d8979于14:07:38由RED副本构建，含RED路径、缺publisher路径，panic1078对应旧源而非最终1080；本次不能验收最终源码。源码候选和受控RED保留，FIX19 blocked/空。A2候选采用publisher独立target，仅复制第三方依赖，重新编译并绑定真实产物后执行原8/15及fmt/strict/audit；一次1800秒（命令1500/root300，含缓存复制与编译）须重新本地Claude准入，未claim。正式127/26、完成26/0不变；六Mega治理由FIX19 C/D、三root Libra治理仅LTR01承载且不push；Grok资格incoming，旧失败/STOP/预算不重置。以下有日期快照按其原时点保留。

> **FIX19受控RED实际收口（2026-10-27T14:11:08.033588+00:00）：** 现行Mega127/Libra26，完成26/0、联合26/153。127/26计划R3本机Claude及root准入已通过；[FIX-MTR-19](../plan/plan-20261015.md#task-fix-mtr-19-服务器签名提交oid的显式对象算法)源码候选与8个新增测试已保存，旧树目标RED已实证并完成资源后检；最终A/自身ER05/版本化C/D仍待，未提交或发布。Libra资格incoming与H1/LTR门不变，Grok原目录只读；Mega六治理由FIX19、Libra三治理由LTR01承载。以下较早候选、待审和失败表述按其历史时点保留；本次新增治理字节仍须B及最终实际审查。

> **127/26正式R2失败后的R3候选（2026-10-27T13:21:34.679269+00:00）：** 现行127/26候选，完成26/0、联合26/153；MTR40→FIX-MTR-15→FIX-MTR-19→FIX-MTR-18→FIX-MTR-20→FIX-MTR-21→FIX-MTR-16→FIX-MTR-22→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。FIX15已发布3abc2558/v0.42.78并实际完整C/D；下一个FIX19仅待本轮H0准入，产品未实施；Grok原Libra db331cd资格incoming，root独立2c87fcd后的三治理由LTR01承载、不push。 R2本机Claude f6fe2748-6ad0-4258-aaaf-da86c2fd59ea实际ER03/GRANULARITY PASS、H0/FIX19_ENTRY/VERDICT FAIL（CLI0、外层1、502输入无漂移；输出7fdbb58eaf65decb788daf4a5a87e933c1e3b47af3d2b50cca8c031463fa4e45）。P1为Mega两个里程碑入口和Libra八处现行字段未前滚，本次逐处修正并扩展全九文件检查。四P2由root处理：旧16/17矩阵/执行总表/追溯归属前滚；六卡明确Out of scope；FIX19 AC7统一任一引用kind不匹配的拒绝谓词；列明update_branch/buck/B3实际签名调用。R1/R2 FAIL原件保留，R3待审。 Mega九治理中的六文件仍由FIX19最终C/D覆盖、Libra三文件由LTR01覆盖；无新RED/A/C执行，旧STOP/预算不变。

> **127/26正式R1失败后的R2候选（2026-10-27T13:08:37.003947+00:00）：** 现行127/26候选，完成Mega26/127、Libra0/26、联合26/153；MTR40→FIX-MTR-15→FIX-MTR-19→FIX-MTR-18→FIX-MTR-20→FIX-MTR-21→FIX-MTR-16→FIX-MTR-22→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。FIX15已实际完整A/B/自身Claude/C/D并发布3abc2558/v0.42.78；FIX19为下一前沿，本轮H0/准入R2待审，产品源码未实施。MTR13 blocked/空；Libra独立2c87fcd之后三治理候选由LTR01承载、不push，Grok原目录db331cd资格incoming且原CI失联FAIL未解。 正式计划R1本机Claude b48d8e35-5a47-44bc-80dd-cd86f5ad55a9实际ER03/GRANULARITY PASS、H0/FIX19_ENTRY/VERDICT FAIL（CLI0、外层1、482输入无漂移）。P1为七处现行旧122/26；本次逐处前滚并保留原文。五P2由root处置：补完整逐hunk diff、127卡执行总表/矩阵/追溯行；FIX18配置方法订为object_format.hash_kind()且处理Result；旧pub签名保留、CL直索引具名债务；FIX19身份入口明确真实Redis/Vault服务。R1输出SHA256 566a522606fb0abd01c79a2da4472f5ad3603c77b495df82307cdb9fd1ae958d。新字节须R2复审，不把局部PASS当总准入。 九治理承载继续Mega FIX19最终C/D、Libra LTR01；旧FAIL/STOP/预算原样保留，尚无新增RED/A/C执行。

> **FIX15实际C/D闭合与原生算法拆分候选（2026-10-27T12:54:03.523026+00:00）：** 现行候选Mega127/Libra26，完成26/0、联合26/153；MTR40→FIX-MTR-15→FIX-MTR-19→FIX-MTR-18→FIX-MTR-20→FIX-MTR-21→FIX-MTR-16→FIX-MTR-22→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。FIX15已实际done/complete（3abc2558/v0.42.78）；FIX16/17重新拆分，18–22 pending，下一实现前沿FIX19须本次ER03/H0实际PASS。MTR13仍blocked/空。 LTR10仍等待H0、QUALIFICATION、H1、WRITERS、LIVE五门。Grok原Libra db331cd交接仍incoming：F2 P1/unknown、原始C/最终资格、来源Claude及受控整合待闭合；CI40484795115的compat-offline-core因runner mi-omarchy失联实际failure，其余7job success，具体失联原因未知。root只读，不重复资格任务。独立Libra2c87fcd之后的三治理只精确本地签名，LTR01唯一C/D承载，不push。 范围审计R1真实本机Claude2419f6ba-2942-4390-94cc-982caacad771于12:43:25Z完成，CLI exit0、审计外层exit1，B3_DEPENDENCIES/GRANULARITY_PROPOSAL/VERDICT均FAIL，419输入无漂移；输出SHA256 0b6a05183375f12e5f41f1cb1f19475cdcd460a946fc2c83c7450c3ee1e6a642。确认FIX16实际7目录及AC低计、FIX17实际5目录、遗漏C段索引/compensator/active-target路径。以下为针对失败的登记候选，尚非新实现、RED/A/C准入；旧e7254190只读PASS不覆盖本次发现。 本次九治理后置字节：Mega六文件明确由下一独立FIX19的I及最终完整C/D承载，不追认被.78旧C覆盖；Libra三文件继续LTR01承载。root唯一写者/串行发布者；不搬运MTR13冻结产品，不复活G13、不重置FIX3三诊断/八A-B及STOP，不免tag Docker D。 下方较早时点的FAIL/STOP、准入及待审记录保留历史含义。

当前已发布支持为FIX15的native读取/物化，加上FIX19的按输入显式算法生成签名提交OID（4986dbac/v0.42.79，完整C/D已闭合）。FIX18的blob索引显式对象算法已通过A3 9/5/1测试、VER5、fmt/Clippy与独立根核验，源码仍未提交，自身ER05 R2 PASS，完整C/D待；提交链/路径树、B3合成、Import与Blake3整链仍按各具名卡边界，不据此宣称完整支持。
> **历史（自身ER05 R2前协议段）：** 当前已发布支持为FIX15的native读取/物化，加上FIX19的按输入显式算法生成签名提交OID（4986dbac/v0.42.79，完整C/D已闭合）。FIX18的blob索引显式对象算法已通过A3 9/5/1测试、VER5、fmt/Clippy与独立根核验，源码仍未提交，自身ER05与完整C/D待；提交链/路径树、B3合成、Import与Blake3整链仍按各具名卡边界，不据此宣称完整支持。
> **历史（FIX18 A3根核验前协议段）：** 当前已发布支持为FIX15的native读取/物化，加上FIX19的按输入显式算法生成签名提交OID（4986dbac/v0.42.79，完整C/D已闭合）。blob索引、提交链/路径树、B3合成、Import与Blake3整链仍按各具名卡边界，未据本patch宣称完整支持；下一源码前沿FIX18，RED1已接受、A1 STOP、A2原窗过期零claim；A3已获用户具名授权、最终执行审待且零claim，GREEN、自身ER05与完整C/D均未完成。

> **历史（A3授权前协议段）：** 当前已发布支持为FIX15的native读取/物化，加上FIX19的按输入显式算法生成签名提交OID（4986dbac/v0.42.79，完整C/D已闭合）。blob索引、提交链/路径树、B3合成、Import与Blake3整链仍按各具名卡边界，未据本patch宣称完整支持；下一源码前沿FIX18，RED1已接受、A1 STOP、A2原窗过期零claim；A3候选待授权/审查，GREEN、自身ER05与完整C/D均未完成。

> **历史（FIX18 RED1/A1状态前原文）：** 当前已发布支持为FIX15的native读取/物化，加上FIX19的按输入显式算法生成签名提交OID（4986dbac/v0.42.79，完整C/D已闭合）。blob索引、提交链/路径树、B3合成、Import与Blake3整链仍按各具名卡边界，未据本patch宣称完整支持；下一源码前沿FIX18，RED/A/C尚未claim。

> **历史（FIX19发布前原文）：** 当前已发布支持仍为FIX15的native读取/物化；FIX19签名源码A/B与产品ER05已核，但C1实际STOP、0.42.79未发布，FIX19 blocked/locally-accepted，用户已授权21库精确清理及C2上限180分钟；当前仅一次6000秒（命令5400/root600）候选；C1新21库已按授权精确清理、原107库不变且清理前H0 R8 PASS；清理后H0 R9 FAIL/P1=3与R10 FAIL/P1=1保留；R11/R12三PASS仅覆盖各自旧冻结字节，Libra合并R1 FAIL/P1=1、R2四PASS/P0=P1=0仅覆盖合并冻结字节；联合H0 R13 FAIL/P1=1保留，R14三PASS/P0=P1=0仅覆盖本次增量前冻结字节；最终联合H0以R15实际审查终态为准，C2未claim，C2执行审与完整C/D仍待；索引、链/树、B3、Import及Blake3边界不变。 **历史（清理前、授权前）：** FIX19-C2一次6000秒和21库清理待授权与新审，完整C/D仍待；索引、链/树、B3、Import及Blake3边界不变。 **历史（C1前原文）：** 当前已发布支持为FIX15的native读取/物化；FIX19签名源码已取得A/B locally-accepted和产品ER05 PASS，治理纠错审及完整C/D待，尚未发布。索引、链/树、B3及Import后继切片仍各自待验收；实际SHA1/SHA256矩阵按各卡执行，Blake3仅进程内验证，不宣称标准Git协议支持。 **历史（确定性治理纠错前）：** 本轮实际支持仅FIX15的native读取/物化；后续签名、索引、链/树、B3及Import切片均未验收。实际SHA1/SHA256矩阵仍按各卡逐项执行，Blake3只作进程内验证，不冒充标准Git协议支持。

> **FIX-MTR-15自身ER05通过与C前置记账（2026-10-27T10:59:24.365358+00:00，版本化C/D待）：** 现行Mega122/Libra26，完成25/0、联合25/148；MTR40→FIX-MTR-15→FIX-MTR-16→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。FIX15源码、A及增量B已核，自身本机Claude R1 ER05/H0/VERDICT三PASS；现in-progress/locally-accepted，版本化全量C/D待；MTR13 blocked/空，FIX16/17 pending。 LTR10仍分别等待H0、QUALIFICATION、H1、WRITERS、LIVE五门；root已只读核Grok交接db331cd，资格仍incoming，F2 P1/unknown、原始C/最终资格、来源Claude及受控整合证据未闭合。原Libra由Grok独占；Libra独立692464e后的三治理只本地签名、由LTR01承载，未push。 本机Claude自身R1 session2573ef00-d676-4597-acce-08e26034bda7明确ER05/H0/VERDICT三PASS、P0/P1=0；root已核1056最终输入SHA/mode及实际exit0采纳。post-A3增量B真实PASS，收据SHA256 5a771b78ab2fdb845eb437c6a5783430d75be72938ee81e72fa4e2de29da551c。A3完整target6/24case、pack170、fmt/strict/VER6 exit0，lib10按779原运行时pin carry，root原窗口3102.229/3600秒完成且107旧DB及所有自有资源闭合、无手工清理。原A1/A2 FAIL/STOP保留。root具名P2在本次前滚消除旧“B待核”陈述；新九治理必须经增量ER05/H0审查并进入版本后完整C，不能借旧B/旧review冒充已覆盖。R1报告首条把integration目标写成各A阶段同hash是措辞错误：只有五生产文件跨A不变，integration分别经历A2类型和A3 oracle修订，最终实际与A3绑定；原RED函数/前缀不变，最终增量审查须明确订正。P3保留无效parents严格拒绝、未调用TLS旧pub入口债务、未增加tree正文/ID一致性保证；ViewRepo不调用默认traverse，VER5按实际调用说明。C具体执行器仍待真实审查，无bump/提交/推送/tag/Release/D完成声明。 无版本/提交/推送/发布；不扩大Harbor例外，旧FIX3 STOP/预算及历史C2根因未知不变。以下早期准入/待审文字按其时点保留。

> **FIX-MTR-15源码及A后置记账（2026-10-27T10:50:33.541442+00:00，自身ER05/C/D待）：** 现行Mega122/Libra26，完成25/0、联合25/148；MTR40→FIX-MTR-15→FIX-MTR-16→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。FIX15源码候选及自身A/结构B已核，仍in-progress/空，自身ER05与版本化C/D待；MTR13 blocked/空，FIX16/17 pending。 LTR10仍分别等待H0、QUALIFICATION、H1、WRITERS、LIVE五门；root已只读核Grok交接db331cd，资格仍incoming，F2 P1/unknown、原始C/最终资格、来源Claude及受控整合证据未闭合。原Libra由Grok独占；Libra独立692464e后的三治理只本地签名、由LTR01承载，未push。 恢复计划98b6c605六PASS；RED执行R3 eedba716四PASS；A3恢复/ER03/H0/执行本机Claude8aa1b673七PASS。A1编译及记录器FAIL、A2新oracle错误FAIL/STOP均保留；A3按公开原模式验证v0忽略filter/全量及v2实际过滤，未增加v0功能，未减少24场景或放松对象断言。受控RED两例各101/0pass1fail；同一测试在最终A变GREEN。lib10仅从A1按779运行时输入及原始日志精确carry；A3 target6(24真实Git场景)/pack170均0失败0忽略，fmt/strict exit0。全部执行仍受A1原09:58:15.249031–10:58:15.249031Z窗口及末300秒root约束。A root收据`/tmp/fix-mtr15-acceptance-R3-20261027/root-adoption.json` SHA256 `c50da1779118a5377d19209b9929f4beae10a729a69f0ab4b51c553fd3c16adf`；本次后置治理新字节仍待结构B及自身Claude/跨仓H0复审。 无版本/提交/推送/发布；不扩大Harbor例外，旧FIX3 STOP/预算及历史C2根因未知不变。以下早期准入/待审文字按其时点保留。

> **FIX15 A2失败与原协议oracle恢复候选（2026-10-27T10:35:40.817643+00:00）：** 现行Mega122/Libra26、完成25/0、联合25/148；MTR40→FIX-MTR-15→FIX-MTR-16→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。FIX15 A2实际FAIL/STOP，fmt通过、集成6例4PASS/2FAIL、24场景仅4完成，pack170/strict/audit未执行；源码候选未提交，仍in-progress/空，自身ER05/C/D待。MTR13 blocked，FIX16/17 pending。 LTR10的H0、QUALIFICATION、H1、WRITERS、LIVE五门仍分别核验；root只读核Grok交接db331cd，资格仍incoming，F2 P1/unknown及原件、来源Claude、受控整合证据未闭合。root仅写独立Libra692464e后治理，由LTR01承载，不push、不覆盖Grok原目录。 A2执行本地Claude65c90ef4六PASS只准其合同；A2 10:27:26.914510Z claim、10:29:13.139207Z外层exit1，matrix实际exit101；两hash的v0 root blob:none均收到8而测试期望6。root10:30:49独立核107旧DB及自有进程/端口/容器无变化、无人工清理，A2仍FAIL。原件`/tmp/fix-mtr15-acceptance-R2-20261027/terminal.json` SHA256 `a8d38511764c62f68a41d7d3f9deb775c0e8e41962cc8f291945db906590c820`；A1失败、两受控RED和lib10通过不改。失败case的packet/pack未在断言前落盘，已随fixture销毁，不声称已观察其协商或warning。公开4ed7b25的smart.rs无v0 filter广告/解析/dispatch，protocol.md明确仅v2 blob:none，双语guide亦说明v0回退全量；错误在本卡新增oracle把v0视作过滤成功。A3候选维持6例/24场景、五命令一次，v0必须同时证明无filter广告/无filter请求、Git明确忽略提示、精确完整对象及fsck；v2仍精确无blob对象、真实filter请求，三算法lib10仅核未变输入后carry。无新增v0产品功能、无减少矩阵/放松集合/提高timeout。候选及执行器`/tmp/fix-mtr15-acceptance-R3-20261027/`尚未准入或执行，须本地Claude恢复/ER03/H0审查PASS后才应用测试并重新冻结实际输入。原窗口09:58:15.249031–10:58:15.249031Z、命令截止10:53:15.249031Z及root末300秒不变；不足即STOP，不自动另开窗口。 旧FIX3全部预算/STOP、历史C2根因未知保留；无ER05/C/D、提交/推送/发布完成声明。以下旧段保留其时点。

> **FIX-MTR-15 A1失败及同窗恢复候选（2026-10-27T10:15:35.805378+00:00，自身ER05/C/D待）：** 现行Mega122/Libra26，完成25/0、联合25/148；MTR40→FIX-MTR-15→FIX-MTR-16→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。FIX15源码候选已实现；A1 fmt及lib10通过，matrix编译E0277且终态记录器FileNotFoundError，A1 STOP；fixture参数和退出记录器已修，A2恢复/沿用lib10待真实Claude准入。仍in-progress/空，自身ER05与版本化C/D待；MTR13 blocked/空，FIX16/17 pending。 LTR10仍分别等待H0、QUALIFICATION、H1、WRITERS、LIVE五门；Grok独占原Libra资格交接，root未消费、未重复。Libra独立692464e后的三治理只本地签名、由LTR01承载，未push。 恢复计划98b6c605六PASS；RED执行R3 eedba716与A1执行R2 0d669305各四PASS仅准其合同。受控RED两例各101/0pass1fail；A1 lib10/0/0及fmt exit0，matrix未运行、child exit未保存不猜测，外层实际exit1，后续门未执行。root于10:11:46核107旧库owner/oid/comment不变、无残留，未手工清理。A1原件`/tmp/fix-mtr15-acceptance-20261027/terminal.json` SHA256 `ae4be33871f009f221b429be578f517cd2ce07a5a5dfaae4bde85a325e627117`。A2修订只改集成测试一行及记录器，拟沿用未变lib10；原3600秒时钟自09:58:15.249031 UTC开始，绝对10:58:15.249031 UTC截止、最后300秒留root，不重置预算。A2五命令和治理H0待本地Claude复审，本次无新的执行claim。 无版本/提交/推送/发布；不扩大Harbor例外，旧FIX3 STOP/预算及历史C2根因未知不变。以下早期准入/待审文字按其时点保留。

> **恢复计划R2实际采纳（2026-10-27 08:49:51 UTC）：** 本地Claude98b6c605六PASS，root核720输入无漂移；FIX-MTR-15现in-progress/空，准源码及自身测试实施，A/RED/C资源合同与自身ER05仍待。MTR13保持blocked/空，FIX16/17尚未开工；无本轮bump/提交/推送/发布，25/122与0/26不变。以下待审文字为此前时点，原R1 FAIL及预算保持。Grok原Libra独占不变。

> **恢复登记R1 FAIL后的R2候选（2026-10-27 08:42:45 UTC）：** P1为两仓现行H0/状态字段滞后；已前滚并保留旧值为历史，三卡粒度/发布规则未放宽。当前现行122/26恢复候选，完成Mega25/122、Libra0/26、联合25/148；MTR40→FIX-MTR-15→FIX-MTR-16→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2。MTR13 blocked/空，FIX15正式计划/H0待本地Claude增量审查；Libra独立副本692464e及未提交三治理候选，LTR01承载，未push；Grok原目录独占，资格消费待其交接。 正式R2终态前源码不实施；不授A/C窗口。

> **原生算法恢复登记候选（2026-10-27 08:29:16 UTC）：** 新增恢复登记候选Mega122/26、完成25/0、联合25/148；当前前沿FIX-MTR-15（正式计划/H0待Claude，未实施），MTR13 blocked/空。串行MTR40→FIX-MTR-15→FIX-MTR-16→FIX-MTR-17→MTR13→FIX-MTR-14→MTR39→MTR92→MTR12/H1→LTR10→LTR11→LTR12→LTR01/H2；各FIX独立patch/自身完整C-D，不聚合、不提前搬运MTR13。Grok独占原Libra资格交接，root未消费；LTR10五门与LTR01发布边界保留。 MTR13 SHA1 R5仅旧源32场景PASS，SHA256 R1/R2真实FAIL；最新strict Clippy R2 exit0只覆盖其冻结输入，不补完整验收。scope审查f8840a43/e7254190有限PASS不代表本次计划准入；所有旧失败/STOP及预算保留。

> **MTR13 ER10外轴阻断（2026-10-27 07:57:31 UTC）：** SHA1 R5实际32场景PASS；SHA256 R1/R2在native fixture克隆阶段实际FAIL，R2定位converter.rs:896按worker线程默认Sha1解析64位tree OID而panic。MTR13 blocked/空，候选保留；必须另登记/审查前置FIX，当前只读审计最小闭合范围，未改native源码、不削减SHA256覆盖。正式恢复DAG/H0待修订与审查，不据旧PASS准入。完整验收/最终ER05/C-D与发布仍待，正式完成数25/0不变；Grok继续独占原Libra。

> **MTR13矩阵R4失败后测试oracle修订（2026-10-27 07:35:19 UTC）：** 源预审 `c919519d` 四PASS仅静态；实际R4 exit101，incremental v0首包通过后normal v0未请求include-tag，错误的全mode协商断言失败。现按真实wants保留精确E/ODB，具名正控仍强制协商，新增仓外packet/PACK留存及完整have集合oracle；R5正在执行。未取得完整A/B、最终ER05或C/D，未提交/推送/发布；旧失败保留。既有广告缺陷登记为open债务DEBT-MTR13-V2-PEEL，后续单轴FIX须另审。Grok独占原Libra不变。

> **MTR13 tag ER03 R2有限准入PASS（2026-10-27 07:01:21 UTC）：** 本地Claude `b501fc98` 五PASS、P0/P1=0；root复算32项输入并具名采纳八P2。准许在原同轴5生产文件/4落点I中实施，不等于完整A/B、源码ER05或C/D。commit预算诊断R1实际1PASS；其它诊断/失败按原件保留，未提交、推送或发布。原Libra仍由Grok独占，未消费资格交接。下方此前候选/FAIL均为历史时点。

> **MTR13 tag计划R1失败后R2候选（2026-10-27 06:53:21 UTC）：** 本地Claude R1 `b169ea17`明确ER03/TAG_SEMANTICS/VERDICT FAIL（P0=0、P1=1）；当前仅前滚精确pack/ODB集合、tag负控与八P2，新增接口源码未实施，R2待审。ref容量R1真实exit101，v2列表计数8196/预期4098，尚未判根因；读失败R1与ref race R1各1PASS仅端点诊断，race仍须成功正控。MTR13完整A/B、ER05、C/D、提交/推送/发布均待；旧FAIL/STOP预算保留，Grok独占原Libra交接工作区。

> **历史候选：MTR13 tag协商修订（2026-10-27 06:38:26 UTC）：** 当前源码编译exit0且哈希无漂移；SHA1矩阵R3真实exit101，HTTP v0 shallow缺失annotated tag，原始日志 `/tmp/mtr13-matrix-diag-R3-20261027/raw.log` SHA256 `74f7e357f513c1121b200ef1054749931187ab4721525d566c2faac812f26d6f` 保留。标准Docker Git v0/v2浅克隆均保留该tag；拟在原code-only pack行为轴加入请求级include-tag传递，I新增pack/mod.rs，规模4落点/5生产文件/M，独立patch边界不变。新的ER03及本地Claude计划审查通过前不实施新增写集；本卡完整A/B、自身ER05、C/D及提交/发布均未完成。原FAIL/STOP及预算不变，Libra资格交接仍由另一Agent负责。

> **历史快照：MTR13实施中（2026-10-27 06:08 UTC）：** ER01/02/03及本地Claude入场审查 `92d5c3d0-7347-4488-b4b5-5347ad8a1c54` 五项PASS，root采纳收据 `/tmp/mtr13-entry-20261027/root-adoption.json` SHA256 `13686c6095f82b667bba3a948d88ec047eb6b11e69d4c9279b2ab2dfe5740428`；MTR13为in-progress/空。受控旧树泄漏RED及旧局部修复GREEN只覆盖当时输入；扩展矩阵R1/R2因测试夹具假定客户端保留pack而exit101，原件分别为 `/tmp/mtr13-matrix-diag-20261027/terminal.json` SHA256 `a5cadb3210aca44cc1c828680cc25c7126cf5680f2008b0143347ad990ee67db`、`/tmp/mtr13-matrix-diag-R2-20261027/terminal.json` SHA256 `30051d4047c7f70f071f292ea96bec37d70562daa88f34eb8dfd81971ac89ba0`。原始接收流oracle修订尚未验收。MTR13全部AC/VER、自身ER05、C/D、提交及发布均待；C资源合同仍须事前另审。Libra资格交接由另一Agent负责，root尚未消费；独立副本 `692464e` 未推送。所有历史FAIL/STOP及预算不变，回滚端到端300秒债务仍open。

> **历史快照：MTR13开工核对候选（2026-10-27 05:27:56 UTC）：** 119/26跨仓H0与MTR01治理自身本地Claude R5 `780c9ce6-84e2-46f7-a60a-25b9af1eeff5` 五项PASS、P0/P1=0，root具名采纳；独立Libra副本三治理已精确签名+DCO提交 `692464ea35acd20dac269a732a2f54cec9081dcf`，树与受审字节相同，资格文件/行保留远端，未push，仍由LTR01覆盖。用户指定的另一Agent负责原目录资格交接，root未消费。Mega现行前沿为MTR13的ER01/02/03；本次六治理及卡片事实修订须自身结构B和本地Claude审查后才能实施，不代A/B、ER05、完整C/D或H1。原FAIL/STOP及所有预算不变；MTR13 C前资源合同须事前另审。

> **历史快照：H0 R4采纳与P2处置（2026-10-27 05:21:40 UTC）：** 跨仓治理增量本地Claude R4 `87c47d25-ce39-4c28-b6f8-0e88c8523323` 五项PASS、P0/P1=0，仅覆盖其冻结输入；B-R4 `/tmp/mtr40-cross-repo-B-R4-20261027/receipt.json` SHA256 `7c64ee891aa480f2c70599ef3c8bda3a6a1d293ffb5a6080721245e419b63e34` 只证明该轮结构。root逐项处置P2：前滚现行索引；纠正前一历史横幅及修订行的计数笔误，H0 R3实际上是五项PASS（含MTR01_ER05），原历史字节保留；独立Libra副本HEAD/index/三写集后检 `/tmp/mtr40-h0-r4-adoption-20261027/libra-postcheck.json` SHA256 `fd4046f703fa46a713d38c69b842e042e55bfd31a8400246b7ea69510e97c090`；资格交接表述改为另一Agent负责、root未消费。本次新字节须结构B及本地Claude复核，未通过前不授MTR13开工；Libra三治理由LTR01承载，原目录由另一Agent负责。旧FAIL/STOP、资格文件及五门不变。

> **历史快照：H0 R3审查采纳与独立副本边界（2026-10-27 05:16:33 UTC）：** 119/26跨仓H0 R3真实本地Claude session `3628ca85-bb51-4dfd-8ae5-9d6feed88a57` 四项PASS，P0/P1为0；仅覆盖当时冻结输入。root具名处置两项P2：两日期计划补04:33:58修订行；结构B-R3 `/tmp/mtr40-cross-repo-B-R3-20261027/receipt.json` SHA256 `e4874305decda6eab0f052c0ec1858695753e67a3c7d536c302f1af2a6db18b9` 对应119/26、25/0、165节点335边及778链接，仅证明其当时结构。本次九文档事实与边界新字节仍须重新结构B和真实本地Claude联合增量审查；未审通过不授MTR13开工。当前H0候选中的Libra `86553fc` 属独立副本 `/media/eli/data/libra-h0-sync-20261026`，不代表原目录HEAD；原目录当次rebase为 `ecd6150`，现由用户指定的另一Agent负责资格交接，root不在该目录写入，不重复其任务。资格文件/行继续保护，LTR10五门、LTR01承载、旧FAIL/STOP和预算不变。

> **历史快照：H0 R2审查与P2处置（2026-10-27 04:33:58 UTC）：** 119/26两仓实际冻结输入的本地Claude H0 R2 `66282b4f-b4f1-4ec8-88ec-574e1e83f2af` 四项PASS，root核33项输入并采纳；此前H0 R1 `32332df7` FAIL保留。本次仅处置R2的六项具名P2并补事实记录，九个本地治理文件仍未提交，新字节须结构复扫与真实Claude增量复审，不能用R2 PASS覆盖新字节。上次实际结构B原件 `/tmp/mtr40-cross-repo-B-R2-20261027/receipt.json` SHA256 `b945bd1423d520e3839913b307bff620fda2330eab69a40bf25e42c6cd775892`，绑定当时119/26、25/0、165节点335边及10个指向受审副本的跨仓链接。Mega MTR40完整C/D；Libra双父86553fc已整合，资格原文保留，三治理仍由LTR01承载、未push；资格交接另由用户安排的Agent处理。

> **历史快照：H0首轮FAIL后前滚（2026-10-27 04:26:54 UTC）：** MTR40 `4ed7b25` / `v0.42.77` 完整C/D已核；Mega登记候选119卡/25 done、Libra26卡/0 done。Libra本地双父signed+DCO提交 `86553fcea658c789ae61bfca7e3cba39d83cee09` 已非破坏整合d04与远端380，资格文件及其状态行与远端逐字一致；三Libra治理尚未提交，LTR01承载、未push。119/26 H0首轮本地Claude `32332df7-effe-49a7-a8fd-b3d37efd6e1b` FAIL（登记和发布边界PASS，H0现行字段两项P1）；本轮按真实终态统一两仓现行字段、保留旧时点记录并处置P2，最终增量审查待，不授后继开工或声称H1/资格完成。旧FIX3 STOP与预算不变。

> **历史快照：登记复审R2后的修订（2026-10-27 04:12:53 UTC）：** Mega实际登记候选119卡、25张done，Libra26卡、0张done；结构B-R3确认为165节点335边无环。登记复审R1 `197bf484`、R2 `5dfe38c4` 均FAIL，R2确认责任卡登记PASS但发现里程碑旧链，现前滚修订并保留失败；六Mega治理仍未提交，当前字节须新本地Claude复审，两仓H0及Libra整合仍待。原FIX3 STOP/预算、MTR40真实完整C/D与资格另机责任不变。

> **历史快照：登记增量修订（2026-10-27 04:04:58 UTC）：** 实际B-R2扫描Mega119/Libra26、完成25/0、165节点335边无环；FIX14方案R7五PASS，实际登记审查R1 `197bf484-bdc7-434f-8d5c-843e80ba1715` FAIL，原失败保留。本轮前滚现行状态索引与MTR13→FIX14→MTR39依赖文字、逐项处置P2；本地六治理尚未提交，新的最终审查及Libra整合后H0仍待，不授后继实施。MTR40实际C/D完成；不重置FIX3预算或声称数据库历史根因。

> **历史快照：FIX14登记候选（2026-10-27 03:59:56 UTC）：** MTR40 C/D完成、六文件收口Claude R3及责任卡方案R7 `3258f432-1803-4b23-b328-21811c4662ca` 五PASS，root分别采纳；R1–R6失败保留。按R7 P2写入FIX-MTR-14、MTR13六治理I、MTR39串行边与窗口交还要求；本轮修改是本地待最终真实Claude审查的治理候选，尚未提交或授权卡开工。实际扫描应为Mega25/119、Libra0/26、联合25/145；两仓H0新字节及Libra非破坏整合仍待。当前代码前沿先完成治理终审，再MTR13→FIX14→MTR39→92→12/H1。FIX14未知时只能按受审ER10及具名DEFER前滚，不把历史FIX3根因或预算重置。

> **历史快照：MTR40收口审查（2026-10-27 03:50:47 UTC）：** MTR40以 `4ed7b25` / `v0.42.77` 完成全部真实C/D，正式Mega25/118、Libra原计划0/26。本地六治理文件经Claude增量R3 session `51136865-89ec-4390-8217-0a37a591c398` 四PASS、root采纳；六文件仍未提交或推送，候选由MTR13承载，需后续正式I变更与审查。本次按R3 P2前滚整理，增量复审待；MTR13开工前须先登记并审查测试数据库责任卡，该卡候选R1–R5 FAIL保持，未知根因不归因FIX3。Libra本地d04与远端380待非破坏整合，三治理由LTR01承载；full pack保密由MTR13实现和验收。

> **历史快照：MTR40收口增量候选（2026-10-27 03:45:27 UTC）：** 发布提交 `4ed7b25` / `v0.42.77` 的完整C及main/tag D均已真实通过，MTR40 done/complete；Mega25/118、Libra0/26。收口Claude R1 `bc65abe8` 与R2 `caf0445c` 均确认C/D PASS但因状态同步FAIL，原失败保留；本轮补齐现行H0/环境/阶段/MTR01字段，六文件仍未提交且增量复审待。后置六Mega治理拟由MTR13 C/D承载：MTR13的治理I已登记候选，须经ER03和真实Claude审查后才能实施；责任卡候选R1–R4均FAIL，尚未登记，MTR13开工前须完成其登记及审查。Libra本地d04与远端380待非破坏整合，三治理由LTR01承载，qualification文件只读。D核验SHA256 `58f511b03e6ec8deca74eec286b7a9b37595fa10d5b48633378d9e959f4900ea`；不宣称full pack保密或测试库根因已证。

> **历史快照：MTR40实际C/D闭合（2026-10-27 03:39:11 UTC）：** MTR40经自身真实Claude审查、完整本地C、签名DCO提交 `4ed7b25a3922a966c9baad033b1c09ebccdf739a` 与 `v0.42.77` 发布后，main D `40442697837` 和tag D `40442760772` 的双架构及docker合成均实际success；Mega正式25/118、Libra原计划0/26。后置治理状态仅在本地，需复审及合法后续承载；Libra计数待非破坏整合后由LTR01承载。MTR40不提供full pack内容保密，MTR13仍需完成对象闭包；测试DB残留责任卡登记审查是后继实施前P2条件，未知根因不归因FIX3。

> **历史快照：H0与MTR01自身审查事实（2026-10-27 01:49:08 UTC）：** 本地Claude H0 R5及MTR01自身ER05/H0增量R1均四项PASS、P0/P1=0、输入无漂移；MTR01 R1 session `ff56448a-ad5d-43b2-9428-eef13a34d3cb`，manifest `e0a574e801952437a48dadd7f61695e1a850578dbde95ee0352a5db261cb97bb`、output `f8b3a7d438f6556df6a96730de8106f79057146fb0d7d16f045be6cf4dd397ad`。本次审后勾选与事实记账须通过新B及独立增量复核后才提交；自身R1 PASS不代MTR40自己的ER05或任何C/D。正式Mega24/118、Libra原计划0/26。

> **历史快照：MTR01事实编辑时点（2026-10-27 01:42:24 UTC；B-R6实际终态2026-10-27T01:43:06Z）：** 本地Claude H0 R5对冻结输入四项PASS、P0/P1=0、无漂移（session `3e1108ae-c79c-4c0c-8bda-cc5971ba8f48`；manifest `f468629c63bfef8c4d6d1ced30462470bd1cf983377e45d19d0152a98c7528fe`，output `ba6fd7a2f98d80bf6ea466d705e5f726de9a797486b0a894163c6ff28e16cea9`）。本段、P2状态索引及资格事实为审后增量，B-R6已实际通过，仍须本卡自身ER05/增量H0审查绑定当前SSOT；MTR01/MTR40自身ER05、精确提交及各自C/D尚未取得。正式Mega24/118、Libra原计划0/26；MTR40 A3/A4/smoke19及源R4 PASS，仍in-progress/locally-accepted。

> **历史快照：H0 R5候选待审（2026-10-27 01:36:51 UTC）：** 本地Claude H0 R1/R2/R3/R4 FAIL保留；R4发现的两仓H0现行来源列、MTR01 AC3、状态及债务审计指针已重绑/修正。B-R5实核118/26、formal24/0、778链接/全锚点零缺失、164节点331边/144任务238展开边无环；当前候选来源545a3ec/e5424c5，资格文件及状态行与HEAD相同。H0 R5、各卡自己的ER05、精确提交/适用C-D待；不重开历史失败预算。

> **历史快照：H0 R4候选待审（2026-10-26 15:30:51 UTC）：** R1/R2/R3治理FAIL保留；现行Phase0/1与M1、H1事务时限债务披露已修订。B-R4实核118/26、formal24/0、778链接/全锚点零缺失、164节点331边/144任务238展开边无环；跨仓目标绑定候选clone。来源545a3ec/e85470a；资格文件及状态行直接HEAD相等、源码仍与R4/A3/A4/smoke相同。H0 R4、MTR01/MTR40自己的ER05、精确提交/适用C-D均待。

> **历史快照：H0 R3候选待审（2026-10-26 15:09:31 UTC）：** R1/R2治理FAIL原件保留；M0/指针/历史范围修订后的结构B核118/26、formal24/0、778链接目标及全部章节锚点、164节点331边/144任务238展开边均通过。来源545a3ec/ae50414；qualification文件/状态行仍HEAD原文，MTR40源码/测试仍与R4/A3/A4/smoke相同。H0 R3、两卡自己的ER05/精确提交/适用C-D待，无本轮推送。

> **执行记录范围：** 下方带旧日期的实施状态仅为时点历史；协议正文除明确历史段外保持现行规范，当前交接和验收状态取本轮H0列及两计划现行卡，不从旧“当前”段外推PASS。

> **历史快照：MTR-40 源码 R2 失败后的前滚候选（2026-10-26T12:54:43Z）：** 本地 Claude R2 对28个冻结输入给出 SOURCE_ADMISSION/ER03_SCOPE/TEST_ORACLE/VERDICT 四项 FAIL，session `c6e590f8-6314-4d4c-ac05-71efd6de787d`、manifest SHA256 `8537207bfd8a69ec7b702b1af94121ccda003c99c55635753b86ebeeb2d358bf`，无漂移；原审查报告 `/tmp/mtr40-source-review-R2-20261026/review.md` 保留。三项生产 P1 为已存在但不可读的 have 阻断普通 fetch、提前运行 ViewRepo 重型 prepare 钩子、旧引用仓库逐 ref 重复全历史扫描；测试与文档缺口亦登记。当前候选将 have 从 ACK/pack 基础集过滤，恢复原 prepare 发包时序，批量共用提交图并设置数量/时间预算；完整用户指南与唯一契约已明示 MTR-13 前普通 full_pack 实际仍泄露同仓对象。严格 Clippy 候选退出0，GREEN12 正在运行；本轮尚无新本地 Claude PASS、H0、本卡完整 C/D、提交或发布，8AC/VER维持未勾，正式 Mega24/118、Libra0/26 不变。

> **历史快照：MTR-40 最终源码候选待审（2026-10-26T12:38:00Z）：** 原源码预审 Claude R1 明确 FAIL 与旧 GREEN7 过度证据保持历史。未知本地 `have`、按 ref 污染隔离、traces-first HEAD、真实共享 blob、污染后 trace 根提交 404、receive-pack 显式删除修复均已写入既定行为轴；GREEN11 对最终当前源码的精确 `code_ref_boundary_` 退出0、1/1 PASS、0 FAIL、36 filtered，日志 SHA256 `1a210b6d7cd3556856e830fb7597c54d5376d91f52b75f3581d3ccc236cf1adb`、terminal SHA256 `961f51daf79e994cfdea287396af121a162e7e0e491ba7304866e8ac050455bc`，测试库残留0。v0/v2 hook 单测4/4通过，nightly fmt退出0；严格Clippy首次因两个可折叠if退出101，原日志保留，修复后严格原命令退出0。任务卡I新增同轴 `src/ceres/pack/mod.rs` 并同步粒度/追溯，8AC及VER仍未勾选，待本地Claude对最终输入重新审查；跨仓H0、完整C、签名提交、推送和D均未取得，正式Mega24/118、Libra0/26不变。MTR-13完整PACK闭包仍另行验收。

> **历史快照：MTR-40 源码审查修复中（2026-10-26T12:20:58Z）：** 本地 Claude 源码预审 R1 对冻结输入明确 `SOURCE_ADMISSION: FAIL`、`TEST_ORACLE: FAIL`、`VERDICT: FAIL`，两项生产 P1 是未知本地 `have` 被误拒、污染分支拖垮整库；测试 P1 是旧共享 blob 夹具不成立。原 R1 报告 `/tmp/mtr40-source-review-20261026/review.md` 与旧 GREEN7 保留为历史输入；GREEN7 不覆盖随后源码和测试。修复候选已追加真实共享 blob、traces 先建仓 HEAD、未知 `have`、按 ref 污染隔离及显式删除修复；GREEN8 在其当时输入 1/1 PASS，GREEN9 对新增 receive-pack 显式删除修复的冻结输入实际 exit0、1/1 PASS、0 FAIL、36 filtered，日志 SHA256 `e9ef9797a35e942e2d7ef88c7a95a3d667c2aef3ab1fb65c1bc80a499834d752`，测试库残留 0；本次仍须最终源码/文档审查。全部 AC/VER 须在最终输入核验后重判；H0、最终本卡 A/B、自身 ER-05、完整 C/D、提交与发布均未取得，正式 Mega24/118、Libra0/26 不变。MTR-13 的完整 PACK 闭包另行验收。

> **历史快照：MTR-40 实施候选前沿（2026-10-26）：** MTR-11 已以 `545a3ec28bc1d2e5b88402f15a04ce143ba8b66c` / `v0.42.76` 完整 C/D 发布。MTR-40 的 ER-01/02/03 来源准入及本地 Claude R1/R2 计划审查已通过，卡为 `in-progress` / 空；在既定 I 内完成公开 binary + Docker Git v0/v2 黑盒候选，精确 `code_ref_boundary_` VER-1 现 1/1 PASS、0 failed、36 filtered，最终当前源码日志 SHA256 `d5f894cca841dc71ac9a70cab72648e9209e776358bfb104d34182356dc5cdd0`，测试库残留 0。旧广告 RED exit101、traces-only want RED exit101、v2 测试入口错误的 FAIL exit101 均原样保留；nightly fmt 与严格 all-target/all-feature Clippy 当前源码 exit0。此 focused 结果仅覆盖代码 ref / HEAD、已知 traces commit/tree/blob want、have、混入祖先 409 和共享 blob；MTR-13 的完整 PACK 对象闭包仍待，不能把本卡候选视作完整隔离。跨仓 H0 两仓终态同步与本地 Claude 审查、本卡 B / 自身 ER-05、版本后完整 C、提交、推送、tag、Release 和实际 D 均未取得；正式 Mega24/118、Libra 原计划0/26、联合24/144不变。Libra 单文件交接 `a4d84829d16205ce26317c6c9b8cdfc55e2ac4aa` 已在远端 main 祖先，资格实施由另一机器继续，原 FIX-MTR-03 失败预算及 STOP 不变。

> **历史快照：MTR-11 产品发布 C/D 已闭合，后继 MTR-40 待准入（2026-10-26）：** 原 A1 的 SHA-256 标准 Git v0 fetch HTTP400/exit101/STOP 保留；A2 七项 VER 与本地 Claude 自身 ER-05 最终 R3 均已通过。0.42.76 最终树 C1 实际 exit0/2365.709秒，nightly fmt、严格 all-target/all-feature Clippy、`source .env.test && cargo test --all`、`cargo build`、`cargo build --tests` 全通过且零编译诊断；21 targets 逐身份 3150 PASS/0 FAIL/3 原 ignored，两项新增 SHA-256 协商 unit 均 PASS。独立根审计在 2422.699/3600 秒内接受，23 个本轮测试库按归属精确清理，重建 binary 实测 `mega2 0.42.76`；C 原件备份 manifest SHA256 `e51cb978d7d4230af2b6cada22ab343c8d5098c9289ca4836700f691471b2144`。本地 Claude C1 执行前资格审查六 PASS，P0/P1=0；signed+DCO 精确 20 路径提交 `545a3ec28bc1d2e5b88402f15a04ce143ba8b66c` 已推 main，签名标注 `v0.42.76` 与手写 Release 已核。main Harbor `40307305036` success、tag Docker `40307391425` 的 ARM64/AMD64/manifest 三 job 全部 success；D 收据 SHA256 `bb2911576a284960863a939138e68a89b752546a15068c35d08075dc13957fc7`。MTR-11 `done/complete`，正式 Mega24/118、Libra 原计划0/26、联合24/144。当前后继为 MTR-40，ER-01/02/03、来源树、H0 和独立验收/审查/C/D 均待，不以本卡 D 代替。Libra 交接提交 `a4d84829d16205ce26317c6c9b8cdfc55e2ac4aa` 仍在远端 main 祖先，资格产品由另一机器负责；原 FIX-MTR-03 失败预算/STOP 不变。本段为 v0.42.76 发布后新增治理字节，未被该版本 C 覆盖，须由后续受审发布树承载；以下“C 待启动”段是其时点历史快照。

> **历史快照：MTR11自身ER-05通过、独立版本C待启动（2026-10-26）：** A1原SHA-256标准Git v0 fetch HTTP400/exit101/STOP保留；A2在1345.015/3600秒内全部本地门exit0且root于1377.665秒内核原件接受，HTTP Git2/2、关闭态3/3、完整模块7/4/15、普通Git/LFS smoke19/0/0、接近1GiB推送HTTP200且连续采样峰值1,150,152,704字节、最终VmHWM1,147,342,848字节，均≤4GiB。MTR11自身本机Claude ER-05 session`90fa13a9-e493-454f-99fc-ed6db1b2de62`实际ER05/H0/VERDICT三PASS，P0/P1=0；root具名P2：新增两unit由最终C全量逐名执行，版本bump后重建新bin，仓库只留脱敏摘要。任务卡现`in-progress/locally-accepted`，8AC/7VER本地勾选；版本后完整C、签名DCO提交、main/tag/手写Release及实际D均未开始，不标done/complete。正式Mega23/118、Libra原计划0/26、联合23/144；Libra交接a4d84829已在远端main且资格产品由另一机器负责，旧FIX3失败预算/STOP不变。

> **历史快照：MTR11 A2本地验收及根审计通过（2026-10-26）：** 原A1 SHA-256 v0 fetch HTTP400/exit101/STOP原件不变。经恢复计划Claude R2、源码R3、执行器R1各自四PASS后，A2于08:46:34.849222 UTC独立claim，1345.015/3600秒全部原定阶段exit0；root于1377.665秒内核原件接受。HTTP Git两hash2/2、关闭态3/3、三个完整模块7/7、4/4、15/15、nightly fmt/strict Clippy、当次release构建、普通Git/LFS smoke19/0/0均通过；接近1GiB解码推送HTTP200且18.122秒完成，进程RSS峰值1,150,152,704字节≤4GiB，资源后检无新DB/未知监听/存活自有进程。新增两unit按root具名P2留待最终C全量首次执行，不能当作已跑。MTR11自身ER05、版本后完整C、签名提交、main/tag/Release及远端D仍待，卡in-progress/空；正式Mega23/118、Libra原计划0/26、联合23/144。Libra交接a4d84829在远端main、资格由另一机器负责，旧FIX3预算及STOP不变。

> **历史快照：MTR11源码R3已准入、A2仍待执行器审查（2026-10-26）：** 本机Claude源码R3 session`894d0a09-8ee7-4575-8d55-cb4194437c5e`对当前源码/测试输入给出SOURCE_ADMISSION/ER03/H0/VERDICT四PASS、P0/P1=0；原R1/R2各自PASS仅覆盖其当时输入。R3两项P2已由root补记receive直接解析测试及具名接受新增unit在最终C全量`cargo test --all`首次运行，A2黑盒仍按原7VER全覆盖。源码目前是未编译/未验收候选；A1 SHA-256 v0 fetch HTTP400/exit101/STOP原件保留，A2尚未claim，执行器/自身ER05/完整C/D/提交推送发布仍待。正式Mega23/118、Libra原计划0/26、联合23/144；Libra交接a4d84829已在远端main，资格由另一机器负责，旧FIX3预算/STOP不变。

> **历史快照：MTR11源码R2与receive解析补强（2026-10-26）：** 本机Claude源码R2在当时输入给出SOURCE_ADMISSION/ER03/H0/VERDICT四PASS（session`bb81fe6b-70a2-447a-aa7e-eb665919ff0f`），P0/P1=0；其P2-1指出receive缺格式HTTP负例与空PACK错误可能同为400。本轮已在既定`http.rs`测试内增加receive命令直接解析的有/无格式对照并nightly格式化，修订后源码R3待审，原R2 PASS不覆盖新行。P2-2具名留待完整C的`cargo test --all`运行新增unit；A2本卡7项VER的真实Git黑盒先验证产品路径，不改卡片计数或缩小覆盖。P2-3将Libra旧H0段标历史。A1失败STOP原件不变；A2、自身ER05、C/D、提交/推送/发布皆未完成，正式Mega23/118、Libra原计划0/26、联合23/144，Libra资格仍由另一机器负责。

> **历史快照：MTR11 v0修复源码候选与复审（2026-10-26）：** 同卡ER-10恢复计划Claude R2四PASS已核、其三项P2文档修正。`trace_capabilities`仅对v0 upload的SHA-256省略格式放行，原显式错配/重复/未知、v2/receive与OID长度拒绝保持；原黑盒增加raw v0无格式pack oracle及负例，源码已nightly格式化但尚未产品测试。源码预审R1本机Claude SOURCE_ADMISSION/ER03/H0/VERDICT四PASS（session`b27ec94a-4244-48fb-b1ed-aea7ff700c18`），其四项P2已形成补强候选，修订后R2待审；不借原PASS覆盖新增行。A1 FAIL/STOP原件保留；A2、最终自身ER05、C/D、提交与发布均未完成。正式Mega23/118、Libra原计划0/26、联合23/144；Libra资格仍归另一机器，原FIX3预算/STOP不变。

> **历史快照：MTR11 A1恢复R2计划单审通过（2026-10-26）：** 本机Claude session`62b33da2-1224-4732-b2fb-e43ce514cd92`直接读取A1 claim/STOP与v0/v2抓包原件，对其当时计划输入给出ER03/ER10/H0/VERDICT四PASS、P0/P1=0；A1仍为SHA-256标准Git v0 fetch失败、exit101并STOP，诊断不计产品验收。R2三项P2（原件逻辑名、粒度矩阵、A2源码审查前置）已具名修正文档，后续最终输入仍须审；源码修复、A2、自身ER05、C/D与发布均未完成。Mega正式23/118、Libra原计划0/26、联合23/144；Libra交接a4d84829已在远端main且资格产品由另一机器负责，原FIX3预算/STOP保留。

> **历史快照：MTR11 A1失败与恢复R1审查（2026-10-26）：** A1实际claim后，nightly fmt、严格Clippy及测试列表通过；原`http_git_`两hash中SHA-1 PASS、SHA-256标准Git v0 fetch返回HTTP 400/exit101，A1立即STOP，余VER/RSS及C未跑。资源后检未见新DB/进程/监听残留。Docker Git v0/v2隔离抓包仅用于诊断，不算产品验收。恢复提案本机Claude R1 ER03/ER10/H0/VERDICT四FAIL（session 0482fa46-80d8-4c80-8071-9adbdba64803，输入/输出SHA见仓外审查收据），P1为原件读取权限、AC计数、A2绑定窗口；本次已修订待R2。MTR11仍in-progress/空，A2/C未claim，无自身ER05/提交/推送/CI。Mega正式23/118、Libra原计划0/26、联合23/144；Libra资格由另一机器实施，交接a4d84829已在远端main，原FIX3失败预算与STOP保留。共享协议唯一事实源为[traces契约](traces.md)，具体恢复条件见[MTR11任务卡](../plan/plan-20261015.md#task-mtr-11-http-traces-git读写)。

> **历史快照：MTR11执行R2失败与R3修订（2026-10-26）：** 本地Claude执行R2实际EXECUTION/EVIDENCE/VERDICT FAIL、FORMAT PASS（session 75bb3cb6-a79a-4832-87b8-784ca81dc68f；工具66369实际exit1，原件/tmp/mtr11-A1-20261026/review-R2）。原执行R1三个P1已关闭，但新增spawn信号屏蔽被子进程继承，正常清理将失败；本轮以启动期间延迟处理信号、服务原始SigBlk/SigIgn核验、父监督器保留完整150秒清理区间修正候选，并补齐PID/start_ticks、已删除二进制进程识别及失败格式化快照。真实源码R5四PASS不变，执行R3待审；A1/C1尚未claim，原3600秒与全部旧失败/STOP保留。无编译、产品测试、自身ER05、提交、推送或发布；两计划目标及Libra另一机器资格实施边界不变。本段更新下方执行R2待审历史。

> **历史快照：MTR11 R5源码准入通过、执行器修复候选（2026-10-26）：** 本地Claude R5四项明确PASS（session 41ae0f36-8b8c-4cc1-9080-e11fd021eb9d；工具59691实际exit0；原件/tmp/mtr11-entry-20261026/http-candidate-review-R5）；root已核124输入及七生产文件与R4完全相同。两行测试配置修正通过源码/ER03/H0审查，真实Cedar、Git两hash及运行行为仍须A1证明。执行R1四FAIL保留；外部执行器已形成RSS合法配置、claim前格式化授权、逐轮资源所有权与有界清理候选，待具体执行R2真实本地Claude审查。监督器工作截止拟定为原claim后2850秒，预留150秒子清理；外层失败最多180秒等待及180秒所属资源兜底，全部受原3600秒总预算约束，未增加窗口或重试。A1/C1未claim，未编译/格式化/产品测试，无自身ER05、提交、推送或发布；23/118+0/26=23/144及Libra另一机器资格边界不变。下方R5待审记录保留为历史。

> **历史快照：MTR11验收执行审查失败及配置纠正候选（2026-10-26）：** 本地Claude执行R1实际四FAIL（session b2c14ea2-d7fb-4028-b0fc-4b1c91f2e65c；工具38575实际exit1；原件/tmp/mtr11-A1-20261026/review-R1）。三P1为RSS配置组合非法、格式化授权检查过晚、超时/信号下资源清理与归属不足；失败保留，尚未准入A1。root额外发现两hash新黑盒测试同样使用trunk+enforce非法配置，已在原测试I内改为Mono使用review+enforce且无push_auth，静态token使用trunk+token+off，保留配置中的ssh_receive_pack=false；生产代码未变，待R5真实源码/ER03/H0复审。下方R4四PASS是原输入的历史结论，不覆盖本次改动。A1/C1仍未claim，尚无编译、格式化、产品测试、自身ER05或本卡提交/发布；原3600秒预算、失败/STOP及正式计数不变。Libra交接a4d84829刚再次确认已推main且两项CI成功，另一机器资格实施边界不变。

> **历史快照：MTR11 R4源码审查通过、正式验收未开始（2026-10-26）：** 真实本地Claude R4四项PASS（session 52654345-6dfd-4732-ab1b-b8bcb3241917；工具10469实际exit0；原件/tmp/mtr11-entry-20261026/http-candidate-review-R4）。R3三个测试P1已关闭，root重哈希108审查输入及七生产文件，生产文件与R3相同；R2/R3 FAIL均保留。P2由root具名接受：早拒绝请求写失败风险包含≤64KiB编码负例及<512KiB delta负例，任一失败必须STOP；撤销fixture无delta由已恢复pack.window=0保证，错误400不可替代预期404。格式化转换和具体执行器仍须独立真实Claude执行审查。源码尚未编译/格式化/测试，A1/C1原3600秒窗口均未claim，无自身ER05、提交、推送或发布；计数仍23/118+0/26=23/144，资格1/5。Libra a4d交接main/CI成功与另一机器实施边界不变。本段更新下方R4待审历史。

> **历史快照：MTR11 R3失败与测试纠正（2026-10-26）：** 真实本地Claude R3已确认R2三个P1关闭，但新增测试存在三个P1，SOURCE_ADMISSION/VERDICT仍FAIL、粒度/H0 PASS（session d6a2a64d-8095-48e9-ae8c-e73c789cbf59；工具41503实际exit1；原件/tmp/mtr11-entry-20261026/http-candidate-review-R3）。原失败不回改。本轮仅在既定测试文件形成修订：按共享协议允许具备项目读权限的不同principal读取active traces，只有非owner写入与未知scope保持相同404；普通v0代码PACK显式剥离其尾部flush；delta与撤销fixture仅打包new-tip排除published-tip的新对象，避免64MiB早拒绝请求写阻塞。待R4源码审查，尚未编译/格式化/测试；A1/C1未claim，自身ER05、提交、推送和发布均未开始。正式计数及Libra main交接/CI成功事实不变；下方“跨principal拒绝”是先前不准确测试预期的历史记录，以本段的读写区别为准。

> **历史快照：MTR11 R2真实失败与修订候选（2026-10-26）：** 本地Claude R2终态为SOURCE_ADMISSION=FAIL、GRANULARITY=PASS、H0=PASS、VERDICT=FAIL（session 6619e422-9fbf-4441-b947-591094f92618；工具48424实际exit1，原件/tmp/mtr11-entry-20261026/http-candidate-review-R2）。三个P1为启用态普通字面别名的Content-Encoding回归、异步测试内嵌runtime播种token、delta负例错误期待409；均已在原I内形成修复候选，R2失败永久保留，待R3源码复审。新增/加强gzip及两层64KiB边界、绑定的跨principal拒绝、by-scope推送、静态token路径、checkpoint拒绝原因、100-continue确定性接收屏障和64MiB真实流失败fixture；没有放宽协议/超时或增开验收窗口。A1/C1尚未claim，未编译、未格式化、未运行测试，尚无自身ER05、提交、推送或版本发布。Libra交接单文件a4d84829已在main且两项main CI实际成功；DOC02继承FIX03产品C/D不变。正式计数Mega23/118、Libra原计划0/26、合计23/144，资格1/5；另一机器负责Libra资格实现。本段更新下方R2待审历史快照。

> **历史快照：MTR11 HTTP源码候选与验收准备（2026-10-26）：** R1源准入四PASS仅用于既定七生产文件的实现；HTTP接线、两hash真实Git黑盒候选及双语指南现已写入独立publisher，均未编译/未验收。命令/body解析避开普通日志；已核全局解压层次，现有http.rs外层中间件为traces保留Content-Encoding，实际解压延至鉴权后，压缩及展开后的upload命令各64KiB。该细节及新增VER7最大预算RSS登记待本地Claude R2审查；不是自身ER05或A/C准入。当前A1/C1均未claim，原STOP/预算不变，无MTR11提交/推送/Release。Libra交接a4d84829的两项main工作流与10/10任务均于本机04:23:52Z实核success，DOC02仍继承FIX03产品C/D；外部实现1/5、原计划0/26，资格实现由另一机器Agent承接。本段覆盖下方历史的HTTP/黑盒/指南未写与main CI pending快照；下一步完成具体验收执行器和RSS/普通Git smoke资源合同后另审，不把未执行结果记为通过。

> **历史快照：MTR11源码实现前沿（2026-10-26）：** 本地Claude session daea17b8-cb6c-4139-bf17-9fa49e0cac08已实际返回SOURCE_ADMISSION/GRANULARITY/H0/VERDICT四PASS，P0/P1=0；root重算82份输入及928tracked文件后采纳，仅准入MTR11既定七生产文件及随附测试/文档。当前in-progress/Acceptance空；许可池、raw PACK encoder、stored-parser错误映射及receive scope解析为未编译源码候选，HTTP接线/黑盒测试与双语文档尚待。A/C未claim、自己的ER05未完成、无新提交/push/tag/Release。十项P2具名留存，release二进制最大预算RSS登记、固定3600秒A/C、真实smoke19、旧锁等待风险及最终输入审查不省略。源码只改独立publisher，原Mega源码WIP与已推送Libra交接卡未动；Libra资格实现仍由另一机器Agent承接。后续源进度以本段及卡片为准，下方源准入候选快照保留为历史。

> **历史快照：现行收口与执行前沿（2026-10-26，MTR11源准入候选）：** 当前状态（2026-10-26，MTR11开工候选）：MTR103完整C/D已闭合，发布main 5a2b93d9ca5a4071dbb5d1a1a6c7a403742a6102/v0.42.75；当前源码未改、仅治理同步。正式Mega23/118、Libra原计划0/26、联合23/144；外部资格实现卡1/5（DOC01/02另计）。Libra单文件交接a4d84829d16205ce26317c6c9b8cdfc55e2ac4aa已单独提交并推main，原计划不因此完成。 当前依赖与后续门（2026-10-26）：MTR11前驱已闭合，本次源准入候选仍pending/空；当前ER01/02/03及本地Claude真实审查待，A/C未claim。依赖顺序11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2不变。Libra资格由用户安排另一机器Agent承接：FIX01→FIX05→FIX04→FIX02→FIX03；F2先做只读ATTRIB（可unknown），FIX05完整C/D后的同因CLOSE或独立FIX闭合必须在FIX04前；无循环依赖。仅FIX05一次release保留现有ingress两方法dead_code的具名例外仍未授权，不能由main交接推导。FIX04旧C1/F1/F2 FAIL与STOP、FIX02旧C1 STOP、所有旧预算及历史C2根因未知均保留。H0、资格、WRITERS、LIVE与H1仍分别核验。
> 本次九治理逻辑文件仍由MTR11既有I承载（Mega六文件两个副本、原Libra三文件本地/LTR01覆盖）；已推送的Libra单文件交接及两仓源码保持原样。以下旧数字/待审/候选文字按其历史时点保留。

> **历史快照：当前C2终态与C3恢复候选（2026-10-25 20:27:14 UTC）：** MTR102 C2实际FAIL/STOP：fmt/strict及全量原生命令exit0，21 targets汇总3116PASS/3原ignored，但stdout/stderr交错导致一例身份解析断言失败，双build未跑，C未通过。经本地Claude退出R2 PASS与root实际50.468/600秒后检，28本轮测试库人工清理、107原库保留，仅两版本面恢复.73；三源码/A2的37例及自身R3原件保留。现仅提出分流原始日志的RB-MTR102-C3，一次3600秒/命令3000/root600，待恢复、carry、最终自身ER05治理增量和具体执行审查；未claim、未新Cargo、未提交发布。正式21/118、0/26、21/144，外部资格1/4；Libra FIX04的F1/F2与H1/LTR01仍待。 依据与预算见[日期计划](../plan/plan-20261015.md)的MTR-102。下方更早的C2待准入/未claim文字保留为历史快照，不能据此重开已消费窗口。

> **历史快照：MTR102自身审查与C2具体准入前事实（2026-10-25 19:07:25 UTC）：** MTR102实际整合树自身R3本地Claude session f81ea66c-b6cc-4b60-9474-5e15549a542e于2026-10-25 18:45:10 UTC给出ER05/H0/VERDICT三PASS，root实算1144输入并以逐文件HEAD/blob补齐原生diff漏出的计划文件。未变A2的37例限域carry不变，源码不变。独立Libra FIX04失败C1四个未发布版本面已于18:57:47 UTC原生精确退出到b73/.87，七路径失败像/0664模式/stash988ded20/全部源码保持；实际退出R2 Claude session144da72b-958e-46e2-893a-16c289c5ceb4于19:02:37 UTC四PASS，root核85输入及2124+22路径状态后采纳，失败发布占用已关闭。原C1的11阶段0与native四FAIL/root STOP并存，F1/F2 P1仍未修，FIX04仍blocked/locally-accepted；不授新的LibraA/C或聚合。MTR102本机工具/Docker服务/环境与.87 native签名、已发布commit和tag payload已重新核验；具体C2合同现在待审，尚未claim/bump。一次总3600秒不变，命令实际截止3000秒（不超过原3300上限），留600秒root资源后检，依据104实测约306秒开销；不改变测试/断言/并发/内部deadline。当前102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2，95/93→102已满足；正式21/118、0/26、21/144与外部资格1/4不变。旧FAIL/STOP/预算/历史C2根因未知保持，尚无本卡新提交、推送、版本CI或发布。
> 下方带旧日期的待审/运行中字段为原时点记录；本段不预记尚未发生的C2结果。

> **历史快照：MTR102实际恢复与整合（2026-10-25 18:38:27 UTC）：** MTR102恢复R2本机Claude五项PASS（session b6f0d6f9-7ca6-47b2-934a-09554aef0818），root已核1174输入并于2026-10-25 18:32:18 UTC采纳；18:32:25 UTC精确18处整合实际退出0：原两仓九治理、独立publisher六治理及三保存源。publisher以已发布3c9e5f3/v0.42.73为基线，保留其余published blob/mode与原工作区WIP，全部index为空。只carry未变A2的37例及原源码ER05，无需增量A；MTR102恢复in-progress/locally-accepted，最终整合树自身ER05、具体C2执行审查与完整C/D仍待，未claim/bump/commit/push。Libra FIX04 C1外层真实exit0、11阶段exit0及资源闭合已核，但四个重试测试/五次失败的实际Claude审查FAIL；root因存储配置错误回退和reset实际stderr不足两项P1记录STOP，候选0.30.88不得发布。Libra失败不构成102的新前驱；全量10761通过不代替验收，原21 ignored、37运行时覆盖缺口及两旧flake仍保留。正式Mega21/118、Libra0/26、联合21/144，外部资格1/4不变；当前102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2，95/93→102已满足。旧FAIL/STOP/已消费预算保持；Mega六治理由102本次C/D覆盖、其post-C/D事实由103六治理覆盖，原Libra三治理仍由LTR01覆盖。
> 以下带旧日期的候选、运行中或待审段是历史快照；当前状态以本段、当前任务字段及具名终态收据为准。

> **历史快照：MTR102恢复候选（2026-10-25T17:08:05.676522+00:00，仅仓外待审）：** MTR104及三承载卡已实际完整C/D并done/complete，正式21/118、0/26、21/144，外部资格1/4。MTR102恢复候选以最新已发布3c9e5f3/v0.42.73为基线，保存三源码、原A2的37例及自身R2原件；来源审计930输入/105审查输入已核，拟只限域carry未变行为A/源审，不沿用旧C1。候选选择复用干净的独立publisher，精确导入三保存源及六治理；旧工作区/失败/预算保持。新C2候选一次3600秒、命令3300秒，另留300秒实际root资源后检；C后事实由MTR103六治理承载。本候选尚未本地Claude审查/采纳、未合源或claim；Libra FIX04原C1运行期间禁止另一Cargo/Claude。后续102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2及95/93→102不变。

> **历史快照：MTR104真实C/D闭合与当前前沿（2026-10-25T16:55:54.587474+00:00）：** MTR104完整C/D及成员覆盖已实际核验：最终签名/DCO提交3c9e5f329e51d278a5d21edc32ad15a893fac0ef、v0.42.73标注tag和人工Release已发布；最终21 targets/3107 PASS/3原ignored/0失败，fmt/严格Clippy/双build均exit0且0项目warning。main 40210347780与tag 40211994533的amd64、arm64、manifest全部success，未使用Harbor延期。DIAG-MTR-01、FIX-MTR-13、FIX-MTR-12、MTR-104本地正式记录done/complete：Mega21/118、Libra0/26、联合21/144；外部qualification仍1/4。新前沿为MTR102来源/carry/恢复准入→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2，另有95/93→102；102旧C1 STOP保留，新C尚未准入。Libra FIX04原C1正在执行，候选0.30.88尚未提交/发布；原A两flake的真实基线与FIX/DEFER条件仍待。本次仅原工作区既有九治理后置事实，Mega由MTR102后续C/D承载、Libra三治理由LTR01承载；不修改104已发布被测树或Libra运行中资格树，不重置任何旧FAIL/STOP/预算。
> 以下带旧日期的事件段保持原时点事实；当前状态以本段、四卡状态字段与文末闭合收据为准。

> **历史快照：MTR104前驱本地闭合与发布入口（2026-10-25T15:06:18.333018+00:00）：** DIAG最终own R2/本地1a9cd767、组合AF1十阶段192唯一PASS、FIX13最终own R6/本地cca52ccd及FIX12最终own R5 8c66b333四PASS/本地4b763c5均已实际核验；三笔精确signed/DCO提交和独立GPG通过，origin仍b7df/0.42.72，未push。当前执行前沿MTR104自身ER05→另审具体C1→唯一patch完整C/D→102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2，另有95/93→102。两child仍locally-accepted并等待104覆盖，正式17/118、0/26、17/144及外部资格1/4不变。Libra FIX04 A1全量10761/21原ignored/2原生flake、源B/ownPASS已核，C/D待；两flake根因未证，资格完成前真实基线及FIX/DEFER条件保持。原REL-MTR-FIX-01退出、FIX3耗尽预算/所有FAIL-STOP、唯一publisher/no-push及H0～H6其它依赖不变。

> **历史快照：FIX13实际本地提交与FIX12开工前沿（2026-10-25T14:49:01.072868+00:00）：** DIAG-MTR-01最终own R2 PASS及signed/DCO本地1a9cd767、组合AF1十阶段/192唯一PASS/root资源后检均已核。FIX-MTR-13实际自己的R4源ER05 PASS/H0 FAIL保留；R5 fd398e89与最终R6 c0ce5ca0四PASS，精确七路径signed/DCO本地提交cca52ccd，实际GPG416E…E0C及完整tree已核。FIX13仍locally-accepted、C/D待MTR104，未push；当前执行前沿为FIX-MTR-12最终增量ER05/精确九路径localcommit→MTR104自身审查/完整C/D→102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2，另有95/93→102。正式17/118、0/26、17/144不变；外部资格1/4。Libra FIX04 A1/full10761/21原ignored/2原生flake、源B与own e76e3d02 PASS已核，C/D待；两flake根因未证、资格完成前真实基线及FIX/DEFER条件保持。原REL-MTR-FIX-01退出、FIX3耗尽预算/全部旧STOP、唯一publisher/no-push与H0～H6其它边不变。

> **历史快照：AF1实际终态与子卡提交前沿（2026-10-25T14:33:05.791314+00:00）：** DIAG-MTR-01最终自身Claude R2 a3f7bd2d PASS及signed/DCO本地提交1a9cd767已核，未推送。REL-MTR-ENV-01 R7已采纳；组合AF1十阶段exit0、五完整模块101+73+11+4+3=192唯一PASS，root于14:24:46Z核实际新编译产物、原107库名称owner及资源终态，未手工清理。FIX-MTR-13/12各自最终增量ER05与精确localcommit待；MTR-104完整C/D尚未准入。现行链99→DIAG→FIX13→FIX12→104→102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2，另有95/93→102；其余H0～H6不变。正式17/118、0/26、17/144不变，外部资格1/4。Libra FIX04 A1全量10761/21原ignored/2原生flake、源B与本地Claude e76e3d02 PASS已核，仅locally-accepted，C/D待；两flake根因未证，资格完成前按具名P2补FIX/DEFER及真实基线证据。FIX02旧C1 STOP/stash与FIX03后继保持；原REL-MTR-FIX-01退出、FIX3耗尽预算及全部旧STOP不复位。

> **历史快照：REL-MTR-ENV-01实际采纳与组合源准备（2026-10-25T13:23:46.846168+00:00）：** DIAG-MTR-01 D1实际GO，七阶段exit0、root资源/来源后检与ownClaude06c202a4 PASS已核；仅证明受控Request机制，不证明历史Interrupted/交错/C2。REL-MTR-ENV-01切换R1/R2/R3/R4/R5/R6失败原件保留；实际R7 ed5d6a31-7828-40e2-bc13-2442d0932b57 于2026-10-25T13:21:47.060495+00:00 五项PASS，root已采纳；2026-10-25T13:23:18.332588+00:00 在独立legacy clone精确形成四测试文件组合树并同步21份治理。没有新AF1/C claim或提交/push；最终采纳文本自身审查及DIAG精确本地签名提交仍待。现行完整执行链：MTR-99→DIAG-MTR-01→FIX-MTR-13→FIX-MTR-12→MTR-104→MTR-102→MTR-103→MTR-11→MTR-40→MTR-13→MTR-39→MTR-92→MTR-12/H1→LTR-10→LTR-11→LTR-12→LTR-01/H2。另有MTR-95/MTR-93→MTR-102；H0～H6其余原边不变。正式17/118、0/26、17/144不变。Libra外部资格1/4；FIX04恢复R2 session15ccf47d于12:26:41Z四PASS，root采纳及精确恢复12:28:09Z已完成；执行审c8ad29bf于12:37:57Z四PASS，A1于12:38:25.742703Z已claim，终态/自身ER05/C/D尚未取得。FIX02旧C1 STOP及stash988ded20保留；FIX01→FIX04→FIX02→FIX03不替代H1。

> **历史R7候选快照（采纳前）：** **环境修复聚合资格切换R7候选（2026-10-25T13:18:15.748263+00:00，仓外待审）：** DIAG-MTR-01 D1实际GO，七阶段exit0、root资源/来源后检与ownClaude06c202a4 PASS已核；仅证明受控Request机制，不证明历史Interrupted/交错/C2。REL-MTR-ENV-01切换R1/R2/R3/R4/R5/R6实质G13 PASS但ER03/文档/H0总FAIL，原件保留；本R7将MTR01的H0～H6映射、SSOT §1 H0单审、两正文依赖登记与五索引对齐，并补Libra修订记录及显式历史来源说明；旧资格计划文件仍具名保留待FIX03受控整合，仍仓外待审、未正式合源/claim AF1或C。候选完整执行链：MTR-99→DIAG-MTR-01→FIX-MTR-13→FIX-MTR-12→MTR-104→MTR-102→MTR-103→MTR-11→MTR-40→MTR-13→MTR-39→MTR-92→MTR-12/H1→LTR-10→LTR-11→LTR-12→LTR-01/H2。另有MTR-95/MTR-93→MTR-102；H0～H6其余原边不变。正式17/118、0/26、17/144不变。Libra外部资格1/4；FIX04恢复R2 session15ccf47d于12:26:41Z四PASS，root采纳及精确恢复12:28:09Z已完成；执行审c8ad29bf于12:37:57Z四PASS，A1于12:38:25.742703Z已claim，终态/自身ER05/C/D尚未取得。FIX02旧C1 STOP及stash988ded20保留；FIX01→FIX04→FIX02→FIX03不替代H1。

> **历史快照：Libra profile警告恢复H0候选（2026-10-25T12:03:44.446299+00:00，仅仓外未采纳）：** FIX02 C1实际STOP：9阶段exit0、full10761/21原ignored/0retry/0flake保留，release一项项目dead_code涉及两构造器；native build数值退出未记录，安装/提交/发布未执行。root已确认0owned/孤儿/target与实际云16表0/R2空；source审ce6fb162 SOURCE PASS、RECOVERY FAIL两P1由新qualification FIX04候选修订。外部候选四实现卡1/4，顺序01(done)→04→02→03；各自G07全C/D，H1/LTR10/LTR01不豁免。正式17/118、0/26、17/144不变。Mega D1于11:39:55Z实际claim，本H0源仅在D1协调器实际退出、资源closed，且D1自身review（原工具句柄24409）已有实际终态并完成root采纳/收据之后，按当时before hash逐项匹配采纳；任何不匹配先重建overlay并重审；本候选不预判D1 GO或REL eligible、不授新窗口。

> **历史快照：恢复R2实际PASS与R3静态设计订正（2026-10-25T10:39:09.077932+00:00）：** 本地Claude09357ed6四PASS、两P1已闭；root于10:38:02Z保存并精确退出FIX13自己失败的.73两Cargo为.72，其它来源及空index保持。R2五P2由root承接：默认10次退避保守70秒，因此在D1尚未claim前提议RED/502新实验进程90秒，其它30秒/ready10秒、总3600不变；不改产品重试/原单测超时/旧窗口。R3及具体执行器仍须实际PASS；DIAG未claim、REL-MTR-ENV-01仍pending、无新C/合树/发布准入。源码guard相对真实dirty前像，重试类型集合只允许Request/Interrupted；两blocked卡旧依赖标历史。

> **历史快照：恢复方案R2修订候选（2026-10-25T10:33:30.462307+00:00）：** 已保留R1真实FAIL并修订正文依赖/发布/里程碑与严格GO/NO-GO；REL-MTR-ENV-01仍pending、无诊断claim/新C/合树资格。原单本地Claude审查指令继续有效。

> **历史快照：FIX13 C1失败后的新恢复候选（2026-10-25T10:18:00.497984+00:00）：** FIX13 C1已FAIL/STOP（2796PASS/3FAIL/3原ignored，20targets/双build未跑），A174及ownR3仅保存；FIX12 C1 STOP同样保留。新DIAG-MTR-01诊断候选pending、MTR-104聚合候选blocked，REL-MTR-ENV-01仅pending不授合树/发布；source审54cf861e确认proxy缺陷但历史选路待证。当前前沿为DIAG具体准入→机制证据→另审恢复边界，102/103/H1不解锁。正式17/118、0/26、17/144（活动17/117、0/25、17/142）；外部资格1/3。LibraFIX02 A2及自身R2 PASS，独立C1于10:09:46Z开始，.30.88本地待门，未提交/推送/发布。 原REL-MTR-FIX-01仍退出，旧FIX3的3/3、8/8与全部STOP不重置。单审依据用户原话“審查變爲單審，不要使用隔離的 Codex ，使用 Claude 進行審查”；该指令的原始UTC未提供，不虚构日期，本轮于上述UTC继承登记；只用本地Claude命令。下文旧时点原件保留。

> **历史快照：FIX13 own最终审查R3与C准入候选（2026-10-25T09:38:18.648958+00:00）：** Claude session b32370dc-66f2-47e0-aad1-5a8e78ff6cfd ER03/ER05/H0/VERDICT全部PASS；source41c495与原174例资格不变，原A1 STOP保留。C尚未claim；新C执行审查R2须绑定本次最终pre-bump治理与严格被测树=提交树执行器。FIX13未提交/推送/发布，正式17/142不变。

> **历史快照：FIX13 C前最终树一致性修订（2026-10-25T09:29:58.013813+00:00）：** own ER05/H0 R2 session92b5eb48已PASS，原A1计数STOP/零重跑恢复证据不变。root复核模板ER-04第③步“最终待提交树”后，不采纳C执行审查810a9a95的P2-5中C后再改治理再提交的建议；该原审PASS保留，但新的发布执行准入须重新审。C尚无entryclaim/claim、版本仍.72，没有消耗C1窗口。现明确C开始后全部tracked字节冻结直至本卡提交/推送/tag/D观察；被测树=提交树，任何tracked差异STOP，不设治理白名单。后置结果只在实际产生后先保留仓外原件，D终态后由root在既有治理路径本地如实回填，随已登记后继FIX12的治理写集/其最终C/D发布，不单独开证据版本，不改写FIX13被测commit。两仓正式17/116、0/26、17/142及依赖不变。

> **历史快照：FIX13自身ER05/H0已PASS，C输入补齐（2026-10-25T09:08:05.161443+00:00）：** 本地Claude sessione7d022a7-2d91-45a4-8618-a2c318656942 ER05/H0/VERDICT PASS，P0/P1=0。root补真实未截断unified diff（3个hunk，仅局部runner及两负例；源hash不变），前滚两处治理旧状态/40计数为实际子字串73；6项P2逐名处置，完整最终输入仍须C执行准入。A1原计数STOP与恢复收据都保留，174实际例不重跑；.72未bump，无提交/push/tag/Release/C-D。LibraFIX02 A2全量进行中，Cargo发布门继续串行；17/116、0/26、17/142不变。

> **历史快照：FIX13 A1原STOP保留、证据重新核定；自身审查待（2026-10-25T09:01:09.525409+00:00）：** 源码仅validate.rs cfg(test)；nightlyfmt、严格Clippy、RED预期1PASS/1FAIL、GREEN2PASS、错名child零测试被父进程拒绝、两完整模块101+73=174PASS（原目标141与额外commands模块33），库2802及3原ignored身份不变。13命令预期退出、原计数STOP 917.614/3600秒，root后检1449.985秒；107个全部DB名称owner保存、零owned-live/新孤儿。自己的Claude ER05/H0及独立五门C/签名/发布D尚待，未bump/提交/push。FIX12/MTR102原C1 STOP不复位。Libra独立FIX02 A1因实测umask0002违既定0022已STOP：5994PASS/5FAIL/4762未跑，旧15focused/1live只是观察；环境恢复A2候选另审，不冒充通过。正式17/116、0/26、17/142及99→FIX13→FIX12→102→…→H1不变。以下旧时点快照保留。

恢复审查收据：本地Claude 076a6493-a599-474e-b968-13886b6d84e7 ER03/RECOVERY/H0/VERDICT PASS；原terminal FAIL不改，仅独立qualified-A收据认可原命令真实覆盖，没有重跑或新增窗口。

> **历史快照：FIX13 A1计数守卫STOP与零重跑恢复候选（2026-10-25T08:55:00.165386+00:00）：** A1实际13命令均按预期退出，fmt/严格Clippy与RED/GREEN/错名负控符合预期，两完整模块分别101/73PASS；但执行器把原命令`config::tests::`当prefix，仅预期40，实际Rust子字串另匹配commands::config::tests::33，因此末阶段计数守卫FAIL/STOP。917.614/3600秒，候选已还原，尚未接受A。root逐名核73恰等listing全部子字串匹配且含原40，107个DB名称owner不变、零owned-live/新孤儿；不重跑产品、不改测试过滤/断言、不重置A1。本轮只提证据核算更正与carry-forward、待真实Claude恢复/H0审核；ownER05/C-D未完成。LibraFIX02 A1环境STOP保留，A2恢复R2真实Claude四PASS并依独立资源收尾条件启动，不等于Libra验收通过。正式17/116、0/26、17/142及原依赖不变。

> **历史快照：FIX13计划PASS、单文件实现候选（2026-10-25T08:17:31.965470+00:00）：** 本地Claude session0bf7b0d0-68ae-4170-b29a-d33ac362e3f6 ER03/H0/VERDICT PASS，root具名处置P2。旧FIX12失败.73两Cargo前像已保存并精确退出到.72，三测试候选/原C1 STOP与MTR102候选均保留。新独立clone仅validate.rs tests内两负例/局部runner候选，in-progress/空；未格式化/编译/新A/自己的ER05/C，未提交/push/tag/Release。A1控制实验及执行器待实审，Libra A进行期间不运行Mega Cargo。正式17/116、0/26、17/142不变；前沿99→FIX13→FIX12→102→…→H1。新clone签名key416E…是本次生成的新key，不倒记为既有发布key；C时核实际PGP/UID/DCO与tag冲突。以下历史原件保留。

> **历史快照：FIX13独立G07恢复候选（2026-10-25T08:09:01.488404+00:00）：** FIX-MTR-13独立恢复计划待ER03/H0，pending/空；FIX12 A1/ownR2保存但C1 FAIL/STOP，blocked/locally-accepted；102 C1 STOP继续保留。新前沿99→FIX13→FIX12→102→103→11→…→12/H1→LTR10；正式17/116、0/26、17/142（活动17/115、0/25、17/140），外部资格1/3。 来源Claude5df82b11确认当前legacy OAuth测试env污染，时间交错快照缺口保留；不扩G13、不重置任何旧C窗口。新clone从实际published b7df/.72起步，只拟validate.rs测试模块内两负例/局部runner，不带FIX12或102候选。FIX12未发布.73须新计划PASS后保存前像并精确退出；FIX13完整自身C/D后再前滚FIX12、另审其A消费与新C窗口，102更后。Libra FIX02 A1已开始，完整15例及真实D1/R2一例PASS、唯一UUID16表零行/R2前缀空，全量仍待；第三方proc-macro-error2 futurewarning按用户排除。原Libra三治理仅本地LTR01承载，qualification分支不混入。以下日期快照保留。

> **历史快照：FIX12独立C1失败/STOP、原候选保留（2026-10-25T07:57:20.283192+00:00）：** 机械0.42.73后的完整C1实际于2026-10-25T07:51:01.991381+00:00退出1，661.427/3600秒；fmt和严格Clippy通过，full-test退出101，仅库2798PASS/1FAIL/3原ignored，其余20targets与build/build-tests未执行。失败为`config::tests::server_identity_stability_credential_rotation_keeps_server_id`在reload.validate读到已移除`MEGA_OAUTH__CAMPSITE_API_DOMAIN`，源码归属待真实审查，不先算根因已证。原件`/tmp/fix-mtr12-C1-20261025/`，terminal SHA256 25b3996925ef8873d0a0988e5d09b49485ef0faf22cbda71bd6b17cb1be12b72；未重试、未提交/push/tag/Release。FIX12 blocked/locally-accepted，仅保留A1与自身ER05/H0 R2 PASS，不等于C完成。root精确手工清理本次dead PID600133的28个新DB、全部原107库名称owner保留，无owned存活进程/新孤儿/spool，三服务稳定；失败收尾原件`/tmp/fix-mtr12-C1-failure-20261025/`。原MTR102 C1及更早STOP不变；独立Libra FIX02 A1已准入，待/按本机串行启动，不受本次Mega发布阻塞而暂停。正式17/115、0/26、联合17/141不变。下方既有PASS/FAIL按当时scope保留。

> **历史快照：FIX12实际A1通过、自己的ER05待（2026-10-25T07:22:20.378613+00:00）：** 受审A1实际14阶段PASS：nightlyfmt、严格all-target/all-feature Clippy零warning/error；受控RED原network通过/环境探针按预期失败，修复GREEN同两例通过，错误exact的0-child成功被父测试按预期拒绝；最终未插桩三完整模块11/4/3共18例全部PASS，2802库listing及3原ignored逐名核定。root后检在原3600秒内1023.064秒完成，104原DB名称/owner保持、无owned存活进程、未手工清理；四变体fresh=false/产物hash及最终模块binary绑定核实，MTR102候选完整保留。FIX12 in-progress/locally-accepted，自己的本地Claude ER05和完整C/D待，版本仍.72、未提交/push/tag/Release；不是历史C1交错或C2数据库根因证明。Libra DOC PR636/main全部实际D成功，FIX02 helper实现候选但未验收；外部1/3、正式17/115与0/26、联合17/141不变。以下旧FAIL/STOP原样保留。

> **历史快照：FIX12 A1执行审查R1 FAIL、未claim（2026-10-25T06:57:28.993059+00:00）：** 本地Claude session6748d1a5 EXECUTION FAIL/H0 PASS，唯一P1为环境指纹含易变工具调用ID且缺claim前验证；本轮排除具名会话传输字段，增加相同sourced-env preclaim验证。P2补ignored3逐名、实际候选binary绑定、退出后进程观察、中断先回收再恢复、恢复守卫决定PASS；保留内存输出/失败日志，proxy Drop避免双panic；尚未执行的实验barrier界限明确10秒，原2/5秒请求与20/30秒进程界限及A3600秒均不变。原R1输入/FAIL保留，执行R2待；新A尚未claim、未消耗验收尝试。FIX12与Libra FIX02均只有实现候选，未验收/提交/发布，17/115、0/26、17/141、外部1/3不变。

> **历史快照：FIX12计划R3已采纳、实现候选与A1待审（2026-10-25T06:46:17.909186+00:00）：** 本地Claude sessiondd1fe5ec-53ef-4e79-86d3-4353bc4b6b72 ER03/H0/VERDICT PASS，R1/R2 FAIL完整保留；root逐项核60输入并具名处置P2。原102失败的本地.73机械bump已保存前像并精确退出到.72，三个候选源码/旧C1 STOP未变。独立clone FIX12三cfg(test)文件已实现候选、nightly格式化，尚未验收/自身ER05/C/提交/推送/发布；in-progress/空。A1一次3600秒的具体RED/GREEN/错误exact负控与三完整模块18例执行器待本地Claude，未claim。前沿99→FIX12→102→103→11→…→12/H1→LTR10；正式17/115、0/26、17/141、外部1/3不变。Libra main核心10761PASS/21原ignored/0retry/项目warning0；network D仍待，FIX02独立分支已取实际合并源，尚未实施。以下旧日期快照保留。

> **历史快照：FIX12计划R2 FAIL后的R3候选（2026-10-25T06:36:23.586705+00:00）：** R2 session01150932实际ER03/H0 FAIL，唯一P1为里程碑M1/H1漏同步；本轮补齐FIX12完整C/D→102→103→11→…→H1并登记修订历史，R3待。R1其他P1已获R2确认修正。17/115、0/26、17/141、外部1/3不变；源码/版本退出/新验收/提交/推送均未执行，102 C1 STOP及旧失败保留。

> **历史快照：FIX12计划R1 FAIL后的R2候选（2026-10-25T06:31:19.176954+00:00）：** 本地Claude session67966d1d实际ER03/H0 FAIL，阻断项为正文顺序/依赖/发布窗口与修订历史追溯未同步；本轮已前滚修订，R2待。新前沿99→FIX12→102→103→11→…→12/H1→LTR10，计数17/115、0/26、17/141，外部1/3不变。修复设计获R1认可不等于计划总PASS；尚未实现/验收/版本退出/提交/发布。102 C1 STOP与候选完整保留。以下历史不回改。

> **历史快照：FIX-MTR-12独立恢复候选（2026-10-25T06:23:48.226590+00:00）：** FIX-MTR-12代理测试进程隔离为当前候选前沿，ER03/H0待本地Claude；未实施/验收/提交/发布。MTR102 blocked/locally-accepted：A2与自身R2保存，C1 FAIL/STOP，不授C2。 源审session a8cf92a6已确认两测试共享代理env污染缺陷，但C1历史交错仍未证明。拟从已发布b7df855d/0.42.72独立native clone修复，只以Command启动环境传代理、保留原断言/timeout；不带102候选代码。原102 .73仅本地未发布，受审后保存前像并只退出自己的机械bump。新DAG：99→FIX12→102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2。正式Mega17/115、Libra0/26、联合17/141（活动17/114、0/25、17/139），外部资格1/3；新增一张独立FIX不改旧失败/预算/STOP。Libra DOC PR636已合并b73e98a，main Clippy/Rustdoc实际项目warning=0，base其余D待；FIX02/03与原merge政策/H1仍待。Libra三治理只本地由LTR01覆盖。以下带日期内容为历史。

> **历史快照：MTR102 C1真实STOP与只读归属（2026-10-25T06:09:53.994636+00:00）：** MTR102 C1实际FAIL/STOP，blocked/locally-accepted（仅保存A2及自身Claude结论）；0.42.73仅本地bump，未提交/push/tag/Release。fmt/strict Clippy通过，full lib2807PASS/1FAIL/3原ignored，后续20targets及双build未执行。失败是raw-fetch测试fixture首次put_raw的GET运输出错，根因尚未证明；不自动重跑C、不增加超时或减覆盖。 原terminal SHA256 d4bd911237aed2d4390591895933f3802ba5c56cf90752456e22c0f20c61de0e；执行647.571/3600秒后失败。root核source/tool/review终态无漂移、无owned/新孤儿/spool，RustFS实例稳定且raw前缀0；26个本轮零连接DB逐名绑定已退出PID4078417后人工清理，原104个名称/owner保持，不宣称自动回收或历史C2根因。两处测试全局代理环境修改与默认HTTP client读取是待验证并发污染线索，不是已证历史因果。仅只读源码/原件审计及恢复设计可继续，受控实验或新C须先具名审定。自己的Claude最终R2 session4f679d9e-fe60-4e52-bff2-da736cd1cb97 ER05/H0/EXECUTION PASS不能覆盖本次失败。Mega17/114、Libra0/26、联合17/140、外部1/3不变；MTR103/11及H1仍待。Libra PR636合入b73e98a、main D仍待，原三治理仍只本地LTR01承载。以下旧日期快照保留。

> **历史快照：MTR102自身Claude R1通过、最终治理与C合同R2待（2026-10-25T05:43:17.171586+00:00）：** A2实际37例/资源后检通过；自身R1 session8a6087eb-543a-4a5a-8f1a-5bd6d37d64c3 ER05/H0/VERDICT PASS，无P0/P1。root具名接受NoSubscriber每次构建Dispatch造成全局interest重建的P2性能风险：当前无公开transport；MTR11开工必须评估Dispatch::none()，任何修正先核合法I/粒度与真实验收，不提前声称修复；两项并发/created_at测试覆盖限制如实保留。源码/A2字节不变，本轮仅9治理记录，R2与C准入待；未bump/提交/push，102仍in-progress/locally-accepted。Libra DOC PR636已由gh合并b73e98a77ecc7c3db1ea905ab55478785d76c041，13PR检查全绿、10761selected PASS/21原skipped/零retry，Clippy/Rustdoc/core项目warning=0；main D及FIX03覆盖仍待，不增版本。FIX01 done/complete、外部1/3；正式17/114、0/26、17/140与前沿102→103→11→…→12/H1保持。以下日期快照保留。

> **历史快照：MTR102实际A2通过与自身Review前沿（2026-10-25T05:36:56.619767+00:00）：** MTR99 done/complete（b7df855d/v0.42.72）；MTR102 A2实际37例通过、in-progress/locally-accepted，自己的Claude ER05/H0及完整C/D待。原A1与A2 R1 FAIL/STOP保留。Mega17/114、Libra0/26、联合17/140（活动17/113、0/25、17/138）；外部资格1/3：FIX-LTR01 main/release D实际全绿并done/complete；DOC PR636/main与FIX02/03仍待。前沿102→103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2；H1与原merge政策未闭合。 三完整模块4/18/15、fmt/strict Clippy/listing全部通过；root后检1337.390/1800秒，在原05:44:30.433733Z截止内，104原DB名称/owner保持、无owned/新孤儿、未手工清理；工具/config/source均核。源码3文件/7AC/3VER不变，0.42.72未bump/提交/push；本次治理字节尚待自身H0，非已通过的A输入。Libra三文档只本地、由LTR01覆盖。以下日期快照保留历史。

> **历史快照：MTR102 A1实际FAIL/STOP与有界A2候选（2026-10-25T05:26:43.466526+00:00）：** 102三文件源码候选及新增8例已写；A1 fmt PASS、strict Clippy exit101（测试helper缺StorageConnector import），A1时36例尚未执行；无残留worker、104原DB保持、未手工清理。A2 R1真实FAIL（时间戳/工具现核）；R2补正确时间、claim/逐阶段工具与Cargo配置hash守卫及180秒后检余量。另源核SeaORM debug-print会展开首次认证token bind，本卡敏感查询以scoped NoSubscriber隔离，新增真实日志控制对照；3文件/7AC/3VER不变，37例待跑。原A1起点2026-10-25T05:14:30.433733Z、总1800秒/绝对截止05:44:30.433733Z不重置；提议一次A2重跑原6阶段，诊断/修文/Claude审查/后检全部扣原窗口，须真实本地Claude ER03/H0/执行合同PASS后才claim。原FAIL/STOP保留，本次不是自己的最终源码ER05、C或发布PASS；不提交/push/bump。正式17/114、0/26、17/140；103/11仍待逐卡开工，H1不提前。

> **历史快照：MTR11开工发现与G09修订候选（2026-10-25T04:57:04.668267+00:00，真实本地Claude ER03/H0待）：** MTR99已done/complete（b7df855d/v0.42.72，完整C与main/tag D实际success）。源码审查session b3f3d07d的SOURCE_REVIEW PASS/总VERDICT FAIL及5项P1保留；拟拆MTR102可信Mono凭据→MTR103绑定管理→MTR11 HTTP。新总数Mega114/Libra26/联合140（活动113/25/138），正式完成仍17/0/17，外部资格0/3。102/103/11尚未实施或验收；本候选经本地Claude单审明确PASS前不启动实现。H1仍为12，后续11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2不变。Libra DOC-LTR-01已signed 5c449bba推分支/PR636，A/B及自身Claude PASS、PR Clippy/Rustdoc项目warning=0；PR/main门与FIX03覆盖仍待。FIX01合入f33be958/v0.30.87，main全量D尚未终态；原Libra三治理文件仅本地由LTR01覆盖。旧失败、STOP、缺失原件、取消项和原merge签名政策不改。

> **历史快照：MTR99实际收口与当前执行前沿（2026-10-25 04:29:22 UTC）：** MTR99 done/complete：0.42.72源码/A1/自身Claude R2/五门C/签名提交b7df855/main/tag/人工Release及实际D全通过；MTR11待ER01/02/03，尚未实施。Mega17/112、Libra0/26、联合17/138、外部资格0/3；H1与Libra资格/原merge政策仍待。 MTR99五门C实际21 targets/3107 PASS/0 FAIL/3原ignored，local C含postchecks2582.007/3600秒；原资源观察器对并发拼接port行FAIL保留，受审保守superset只读恢复通过。25个本次owned测试DB经精确授权人工删除、原104DB保留，此为测试环境收尾，不是自动回收修复或历史C2根因证明。main Harbor40160748002及tag Docker40160777703的amd64/arm64/manifest四job全部success，D终态2026-10-25 04:26:13 UTC；实际head b7df855d2ca6fb945774b5dd397884ad82b46c31、annotated v0.42.72、人工Release均核。 Libra独立资格clone的DOC-LTR-01已通过A/B，自己的本地Claude审查进行中、尚未提交；FIX01 PR635已合并f33be958/v0.30.87，release与CodeQL成功、main base待。原Libra三治理文档仍只本地、由LTR01覆盖，不混入用户指定下一笔warning提交。以下日期快照保留历史。

> **历史快照：MTR99自身R1源审PASS/H0 FAIL与前滚修订（2026-10-25T02:50:50.524869+00:00）：** MTR101 done/complete：v0.42.71完整C/D；MTR99 A1实际61例通过、自己的R1源码ER05/内存PASS，但H0四处索引P1导致总FAIL；索引已前滚、R2待审，未bump/提交/push/C-D。正式Mega16/112、Libra0/26、联合16/138、外部0/3；Libra资格/H1/原merge政策待。 原R1精确1016输入与FAIL完整保留；A1源码字节不变、不重跑已通过的61例。五项P2具名登记，原FAIL/STOP与缺失证据保持。

> **历史快照：MTR99实际A1通过、自己的源码审查待（2026-10-25T02:39:53.809601+00:00）：** MTR99五文件实现已通过A1：nightlyfmt/严格Clippy、原selector4/3/4及七完整模块7/13/5/6/9/7/14，共61唯一用例0fail/0ignore；root后检496/1800秒，104个DB名称/owner保持、无owned进程/私有spool、RustFS raw前缀仍0，未手工清理。自己的Claude ER05/H0及完整C/D待，0.42.71不变，未提交/push/tag/Release。正式Mega16/112、Libra0/26、联合16/138、外部0/3保持；101已done/complete，Libra PR635 fontconfig及资格02/03、H1/原merge政策仍待。旧FAIL/STOP与缺失原件不改。

> **历史快照：MTR99开工已准入、源码实施候选（2026-10-25T02:06:58.385961+00:00）：** 本地Claude R2实际五项PASS（session0b34282e，终态01:56:01 UTC），root重算54输入无漂移、保存原件并具名处置5项P2；R1 FAIL/STOP与缺失证据保持。MTR99已开始五文件I内实现及真实后端测试候选，尚未执行A/自己的源码ER05/C/D，不勾AC/VER、不bump/提交/push。MTR101 v0.42.71完整C/D已闭合。正式Mega16/112、Libra0/26、联合16/138、外部0/3保持；Libra PR635远端fontconfig阻塞与资格02/03、H1及原merge签名政策未关闭。

> **历史快照：MTR101实际C/D闭合与MTR99 R2候选（2026-10-25 01:50:01 UTC）：** MTR101 done/complete：0.42.71源码/A4/自身Claude/五门C/签名提交6a63504/main/tag/人工Release及实际D全通过；MTR99 R1三P1 FAIL保留，R2候选待本地Claude，未实施。Mega16/112、Libra0/26、联合16/138、外部0/3；H1与Libra资格/原merge政策仍待。 main40150856672及tag40150892586全部四job success，终态01:43:34 UTC。Libra原三已审文档signed+DCO本地7541e9b已核，未push；PR635仍未合并/tag/Release，compat-clippy缺fontconfig。旧FAIL/STOP与缺失原件均保留；下方带日期旧前沿为历史。

> **历史快照：MTR101已发布、MTR99开工修订候选（2026-10-25 01:36:27 UTC）：** MTR101自己的Claude R2 ER05/H0 PASS；0.42.71五门C实际21 targets/3096 passed/0 failed/3原ignored/0 filtered，root资源后检2596/2700秒。signed+DCO `6a635042a40bb4d013c56c8655d9a42558150c83`、main、标注v0.42.71及人工Release实核；main40150856672/tag40150892586正在执行，in-progress/remote-pending。MTR99仅候选：解耦raw读取/parser控制与push时钟，拟5生产文件/3落点/M，8AC/3VER与independent patch/self保持，须本地Claude ER03/H0及101 D成功后实施。Libra PR635未合并/tag/Release，compat-clippy缺fontconfig.pc，资格02/03与H1/原merge政策待。正式Mega15/112、Libra0/26、联合15/138、外部0/3不变；历史FAIL/STOP与缺失原件保留。

> **历史快照：MTR101 A4验收与最终源码审查前沿（2026-10-25T00:34:41.026023+00:00）：** MTR101 A4实际PASS：fmt/严格Clippy、selector6/4/1及四完整模块7/9/16/10全通过，42唯一用例、0failed/0ignored；212.117/900秒，root后检确认输入/工具/104个DB名称与owner不变、无新孤儿、未手工清理。新覆盖已兑现，最终候选自身Claude ER05 R2待；0.42.70不变，未bump/提交/push/C-D。Libra PR635已推但CI缺fontconfig未闭合，未合并/tag/Release；资格02/03与H1前驱不变。 原A1/A2 FAIL/STOP、A3/R1限定输入PASS保留；正式Mega15/112、Libra0/26、联合15/138、外部0/3不变。

> **历史快照：MTR101 覆盖方案采纳与实现候选（2026-10-25 00:20:32 UTC）：** 本地Claude ER03/H0实际PASS，无P0/P1；仅两现有cfg(test)模块补齐候选，生产字节不变。新增storage第二页缺失/异tree拒绝及全匹配正对照，保留At并补Absent/首发无副作用、原checkpoint同一行的真实增量发布，原断言与空tree stale-binding覆盖保持。A4预期42例、一次900秒执行准入待，尚未编译/测试；自己的新ER05与C/D仍待。A3/R1 PASS和旧FAIL/STOP均保留；Libra PR635 CI缺件未闭合，正式计数不变。

> **历史快照：MTR101 自身审查与覆盖补齐候选（2026-10-25 00:12:22 UTC）：** MTR101当前源码自身Claude ER05/H0已PASS、A3实际41例通过；root决定在C前关闭P2-1/2覆盖缺口与P2-6正文滞后。仅两个现有cfg(test)模块拟补断言/一个case，生产源码不改；A4一次900秒方案待ER03/H0审查，尚未实施/claim。0.42.70不变，未提交/push/C-D。Libra PR635 clippy缺fontconfig失败，未合并/tag/Release；其余资格和H1前驱不变。 原A3 PASS、A1/A2 FAIL/STOP及预算全部保留；正式15/112、0/26、联合15/138、外部0/3不变。

> **历史快照：MTR101 实际A3与自身代码审查前沿（2026-10-24 23:59:09 UTC）：** 2026-10-24 23:59:09 UTC：MTR101 A3实际PASS，完整模块7/9/15/10共41例、0fail/0ignore；fmt/严格Clippy通过，32.954/900秒。root核输入/工具/DB/进程无漂移或残留，原A1/A2 FAIL/STOP不改；源码及A/B完成候选，自己的本地Claude代码ER05待，未bump/提交/push/C-D。Libra FIX-LTR01已signed2852497推功能分支PR635，远端clippy缺fontconfig.pc失败，未合并/tag/Release；资格02/03与H1仍待。 正式Mega15/112、Libra0/26、联合15/138、外部0/3不变；以下旧时点均为历史。

> **历史快照：当前恢复与跨仓前沿（2026-10-24 23:51:00 UTC）：** 2026-10-24 23:51:00 UTC：MTR101 A2实际FAIL/STOP；三组selector6/3/1已PASS，外部阶段名校验在首个完整模块Cargo启动前失败，四完整模块均未执行。A3恢复候选一次900秒、ER03/H0/执行准入待本地Claude；源码不变，8AC/4VER未勾。Libra FIX-LTR-01本地C受限接受、自己的Claude PASS，signed/DCO提交2852497已推功能分支并建PR635；远端clippy缺fontconfig.pc失败，未合并/tag/Release。 原A1/A2预算与FAIL/STOP全部保留；旧时点为历史。正式Mega15/112、Libra0/26、联合15/138、外部0/3不变。

> **历史快照：MTR101 A2准入修订（2026-10-24 22:10:09 UTC）：** R1实际EXECUTION FAIL/H0 PASS；执行器串行前置P1已修成R2候选，待本地Claude复审，尚未claim/测试。原A1 FAIL/STOP、源码恢复ER03/H0 PASS与一行夹具修复保留。Libra FIX-LTR-01已bump0.30.87并运行独立C，尚未提交/推送/发布；不与A2并跑测试。下方旧时点保留。

> **历史快照：MTR101恢复方案实际通过（2026-10-24 22:01:36 UTC）：** 本地Claude ER03/H0 PASS，P0/P1无；root采纳受审的一行cfg(test)键隔离修复，8AC/4VER未勾，in-progress/空。A1 FAIL/STOP保留；A2一次1800秒最终执行准入待，尚未运行。原功能链/计数与C/D边界不变；下方候选及旧状态为历史。

> **历史快照：MTR101恢复候选与Libra并行前沿（2026-10-24 21:56:54 UTC）：** 2026-10-24 21:56:54 UTC：MTR101 A1实际FAIL/STOP，chain6/6、publication0/3，resources及四完整模块未运行；原8AC/4VER未勾。仅测试fixture冪等键隔离的ER03/H0恢复候选待本地Claude，A2一次1800秒未准入/未执行，旧A1不重置。FIX-LTR-01源码/A/自身Claude ER05已PASS，C准入待、无提交/push/发布；外部0/3。FIX11已done/complete；Mega15/112、Libra0/26、联合15/138。 当前功能链仍101→99→11→40→13→39→92→12/H1→LTR10→11→12→LTR01；LTR10仍需H0/H1/WRITERS/LIVE及DEP-TR-QUAL-REPAIRS，原merge签名政策未关闭。以下旧时点事实保留。

> **历史快照：FIX10代码审查R1事实与文档订正（2026-10-24，最终输入R2待）：** 本地Claude session `235da2d1-f225-4c03-a85c-e84971911a44`，exit0，ER05/H0/最终VERDICT均PASS，无P0/P1；输出SHA256 `a5380d528ff218fe57c31644581059fd5ad0c172611ad65ce5426610ba1ede2a`。root选择在发布前关闭P2-A～C：订正陈旧RED/GREEN待执行文字，恢复误插FIX10的历史快照原文并保留当前依赖边，对齐Granularity写集措辞；上述变更另行R2审查，源码及A1输入不变，不冒用R1覆盖改后文档。root具名接受P2-D：头长度解析允许前导+但最终原解析器拒绝，至多回原EOF路径；P2-E：缺权限由未改exec入口先拒绝，现有完整SSH进程目标在最终C实跑，早处理不绕过此入口。无新scope/waiver。C/版本/提交/推送/D均未执行。

> **历史快照：FIX10本地A/B结果（2026-10-24，代码ER05待）：** 独立clone单生产文件SSH修复候选完成；旧源handler RED1PASS/4FAIL、真实Docker Git RED0PASS/2FAIL（原90秒期限）已保存。相同新测试GREEN5/5、真实Git2/2、完整SSH模块34/34，0failed/0ignored；nightly fmt、strict all-targets/all-features clippy、release build均exit0。原19项HTTP/SSH/PUSH/LFS smoke 19PASS/0FAIL/0skip，HTTP OpenAPI200/3.1.0/96paths、SSH就绪实测；自有服务exit0、DB/Redis键/容器/目录均清理，无source/runner漂移。A1 terminal SHA256 `bdfd4f90cb34791337ab0dbb84d9eaeae9c70c19db6fa050b304f2ec97b6f330`，原件`/tmp/fix-mtr10-A1-20261024/`；smoke log SHA256 `addaaa247a9809959889bd54deb08267e79c48c195f0e0372afd5a166624a59b`，原件`/tmp/fix-mtr10-entry-20261024/`。原始A/B不替代最终C：本卡尚无代码ER05、版本bump、签名提交、推送、tag/Release或D。原FIX07候选仍冻结/blocked，未随本卡发布。两仓当前完成数仍11/0/11，Libra不push。

> **历史快照：FIX10实现候选（验收进行中，尚未发布）：** SSH receive-pack按pkt-line边界保存命令扫描进度；完整非空命令段以flush结束且无PACK、既有解析器确认全部为删除时，立即进入原receive-pack处理并发送report-status、exit-status、EOF/close。完成标记阻止重复data/EOF再次执行事务或产生missing-flush错误；非删除、混合、畸形或不完整请求仍沿原EOF路径处理，PACK数据不被反复扫描/复制。所有exec鉴权、只读/禁用守卫及main/tags/CAS规则沿用原路径。旧源真实handler RED为1PASS/4FAIL、真实Docker Git RED为0PASS/2FAIL（原90秒期限），根因链已受控复现；GREEN5/5、真实Git2/2、完整SSH34/34及19smoke已过；代码ER05与最终C/D尚待，不能据此记发布成功。

> **历史快照：FIX10计划采纳事实（2026-10-24）：** 本地Claude R2 session `9057479b-c960-44fc-ad17-5f38715acd2f`、exit0，ER03/H0及最终VERDICT: PASS；输出SHA256 `7caa7cc57c97dadf32a5dec7c1499ea80f33405841c209143b3743bf9a2fb6b5`，manifest SHA256 `48fe01cd13f5f2d2545c9c637bdc36b8b7cfc09a67af4e197b67efcaf93f7fad`，九文件SHA守卫应用成功，仅Mega独立发布clone六文件及Libra三文件。原Mega11个FIX07候选完全冻结。R1两个P1已关闭，R1真实FAIL原件/摘要保留于`/tmp/fix-mtr10-entry-20261024/review-terminal.json`；本PASS只准入计划，不是代码ER05/C-D。root负责R2 P2-1～4本批事实同步（待代码ER05核）、P2-5未来整合候选证明原生stash/ff-only行为、P2-6本登记。新测试先运行旧源RED；源码实现、全部A/Review/C-D/提交/推送仍待。

> **历史快照：FIX-MTR-10 计划已采纳（R2 PASS；测试先行，源码修复待）：** 现有普通Git SSH receive-pack只在EOF分派，真实delete-only客户端可能先等待report-status而不发EOF。拟在完整合法且无需PACK的命令flush后执行原鉴权/引用处理并回报，防止重复执行；普通PACK及Traces禁用/只读边界保持。来源和验收见[当前计划](../plan/plan-20261015.md)，不把候选写成已发布行为。

# Git Protocol 兼容性改进计划

## 原生读取对象算法（FIX-MTR-15实现边界）

发布资格以[任务卡及C/D收据](../plan/plan-20261015.md#task-fix-mtr-15-原生读取与惰性物化的显式对象算法)为准。原生advertise惰性物化及upload-pack以monorepo配置算法解析tree/commit，并在首次或tombstone续接时构造同算法提交。full、incremental、shallow、blob:none与直接对象PACK使用该显式算法；协议模式不变：v0不宣告filter，Git带--filter时明确提示忽略并完整下载，v2 blob:none实际过滤blob；无效metadata通过既有错误类型拒绝，错误不包含原始signature正文。

本卡不改变锁/CAS、物化时机或已有对象，不修复存量混合格式对象。原生trunk写入由FIX-MTR-16、ImportRepo往返由FIX-MTR-17分别恢复，MST2 publication与其余TLS债务仍保留；不能从读取测试推导全流程SHA256/Blake3支持。共享协议及跨仓依赖仍以[traces契约](traces.md)为唯一事实源。


本文档记录 `mega2` 当前 Git SSH/HTTP 协议实现的现状分析、主要兼容性问题、风险点和分阶段改进计划，用于提升与标准 Git 客户端的兼容性。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **与其他模块的依赖**：Git Protocol 改进与 config/vault 的认证统一相关。当 config.md 阶段 2（CLI LoadMode）完成后，可统一 HTTP/SSH 的认证上下文设计。集成测试参见 **`integration.md`**。

> **用户可见规则**：Monorepo 仅公开 `main`，Git 客户端不能推送 Tag；路径例外和推送行为见[使用指南](../user-guide.zh.md)。本文负责记录 Git 协议行为与 smoke 矩阵。

## 事实校准（2026-06-14，更新 2026-07-01）

> 本文档中的代码引用已对照当前 `src/` 重新核对。当前 Git protocol 实现处于基础阶段，具有完整的功能框架但多处缺乏错误处理和兼容性完善：
>
> **2026-06-23 更新**：已完成首批 malformed input panic 止血：
> - HTTP `info/refs` 的 query 反序列化错误以及 `service` 参数缺失或非法时，不再 `unwrap()` panic，而是返回 `ProtocolError::InvalidInput`。
> - smart protocol pkt-line 读取新增 `try_read_pkt_line`，对非十六进制 header、短 header、长度小于 header、payload 不完整等输入返回 `ProtocolError::InvalidInput`，并补单元测试；upload-pack 与 receive-pack 命令解析已改用该可失败 parser。
> - upload-pack 的 `want` / `have` object id 长度和 UTF-8 解析已改为协议错误；receive-pack 命令解析会传播 pkt-line/capability 解析错误。SSH receive-pack 遇到非法 command pkt-line 时记录告警并返回 error 文本，不再在该点 panic。
>
> **2026-06-23 更新 2**：已完成 SSH exec parser 止血：
> - `exec_request` 不再用 `split(' ')` 和 `command[1]` 直接索引；新增独立 parser，支持单引号、双引号、反斜杠转义和包含空格的 repo path。
> - 只允许 `git-upload-pack`、`git-receive-pack`、`git-lfs-authenticate`、`git-lfs-transfer`；未知命令、缺 path、引号未闭合或参数数目错误会返回 channel failure 和可读错误，不再默认降级为 upload-pack。
> - repo path 只去除末尾 `.git`，不再删除路径中间的 `.git`。
>
> **2026-06-23 更新 3**：已完成 receive-pack `PACK` magic 分界止血：
> - HTTP / SSH receive-pack 不再搜索 `PACK` 字节序列，而是复用 `SmartSession::split_receive_pack_request` 按 pkt-line command list 的 flush-pkt 分割 commands 与 pack bytes。
> - 新增单元测试覆盖 capability 中出现 `PACK` 不误切分，以及缺少 flush-pkt 返回 `ProtocolError::InvalidInput`。
> - 当前实现仍是完整 body / channel 数据缓冲后再 split；更完整的 streaming pkt-line reader 仍为后续，delete-only push 语义与空命令列表校验已加固。
> - **2026-06-28 更新**：SSH per-channel state 已实现。`SshServer` 不再维护连接级 `smart_protocol` / `data_combined`，而是按 `ChannelId` 维护独立的 `GitSshChannelState`，每个 channel 拥有独立的 `SmartSession` 与 receive-pack 缓冲区。`capability advertise` 已完成首批保守收敛，后续仍需完整 truth table 覆盖。
>
> **2026-06-23 更新 4**：SSH upload-pack 初始响应已删除 `String::from_utf8(...).unwrap()`，改为直接按 bytes 写回 channel；Git 协议 payload 不再在该路径上被 UTF-8 假设约束。
>
> **2026-06-23 更新 5**：HTTP upload-pack request body 聚合不再对 body stream 错误 `unwrap()`，已与 receive-pack 统一经 `ProtocolError::InvalidInput` 返回。
>
> **2026-06-23 更新 6**：HTTP `info/refs` query 已按 smart HTTP 规范收紧为 exactly one `service=...` 参数，额外参数和重复 `service` 均返回 `ProtocolError::InvalidInput`。
>
> **2026-06-24 更新**：SSH `git-lfs-transfer` pure SSH 路径已从普通占位文本 `not implemented yet` 改为通过 SSH stderr extended-data 返回明确 unsupported 错误，并继续返回 channel failure，明确声明不支持 pure SSH LFS transfer、引导客户端使用 `git-lfs-authenticate` 的 HTTP fallback。
>
> **2026-06-24 更新 2**：SSH LFS exec parser 已收紧 `git-lfs-authenticate` / `git-lfs-transfer` 参数语义，operation 从 optional 改为必填且仅允许 `upload` / `download`；缺失或未知 operation 返回 channel failure，不再接受模糊请求。
>
> **2026-06-24 更新 3**：SSH `git-lfs-authenticate` 响应序列化已移除 `serde_json::to_vec(...).unwrap()`，改为错误传播；成功写回 hybrid LFS JSON response 后显式发送 channel success。
>
> **2026-06-24 更新 4**：capability truth table 阶段 3 收尾——为 `side-band-64k` 补充 `build_side_band_format` 专用单测（含启用/未启用两条路径）；为 `ofs-delta` 补充 advertise/parse 单测并明确其 OFS_DELTA pack 编解码由 `git-internal` crate 实现（`internal/pack/decode.rs` 处理 offset delta），mega2 侧仅覆盖 advertise/parse；对 SHA-1 object format 落地显式策略——协议允许 SHA-1 默认时省略 `object-format`，mega2 策略是对 SHA-1-only repo 不 advertise `object-format`，由 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` 锁定。
>
> **2026-06-24 更新 5**：阶段 4 认证上下文统一收口（工作项 1/2/3/6）。新增 `SmartSession::set_authenticated_user`；SSH `auth_publickey` 成功后将 key owner username 存入 `SshServer.authenticated_user`，exec 阶段注入 `SmartSession.auth`；HTTP receive-pack 改用同一 helper。SSH push 的 commit binding 不再匿名。残余：upload-pack 匿名策略、receive-pack repo/path 级 push 权限（工作项 4/5）。
>
> **2026-06-24 更新 6**：阶段 2 delete-only push 落地（工作项 4）。新增 `SmartSession::is_delete_only_push`；`git_receive_pack_stream` 检测到全 delete command list 时跳过 `unpack_stream`/`receiver_handler`，`unpack_result` 视为 Ok，直接处理 ref 删除并返回 report-status。由 `is_delete_only_push_detects_pure_delete_vs_mixed` 锁定。
>
> **2026-06-30 更新**：LFS lock / metadata storage 路径完成 panic 止血。`LfsDbStorage` 的 object/lock CRUD 不再 unwrap SeaORM 结果，而是返回 `MegaError`；`ceres::lfs::handler` 的 lock list/create/delete 路径不再 unwrap lock JSON、limit parse 或 DB 结果，统一映射为 `GitLFSError`。`Link::new` 的 86400 秒过期时间也改为 infallible chrono 构造。
>
> **2026-06-30 更新 2**：LFS router response/body 路径完成 panic 止血。`api::router::lfs_router` 不再通过 `Response::builder().body(...).unwrap()` 构造静态 LFS JSON/stream responses，改为 `Response::new` + 静态 header；upload object request body 聚合错误也从 `unwrap()` 改为 400 错误返回。
>
> **2026-06-30 更新 3**：pack traversal count helper 完成首批 panic 止血。`RepoHandler::traverse_for_count` 与 `traverse_trees_only_for_count` 不再 unwrap tree lookup 结果，改为返回 `Result<(), MegaError>` 并由 monorepo/import pack 生成路径向上传播错误。
>
> **2026-06-30 更新 4**：pack traversal entry send 完成首批 panic 止血。`RepoHandler::traverse` 与 `traverse_trees_only` 不再 unwrap pack encoder channel send 结果，blob/tree entry send failure 会作为 storage/protocol error 向上传播。
>
> **2026-06-30 更新 5**：monorepo pack generation 的 encoder startup 与 commit entry send 完成 panic 止血。`Monorepo::{shallow_pack, filtered_pack, incremental_pack}` 不再 unwrap `PackEncoder::encode_async` 或 commit entry channel send failure，统一映射为 `MegaError` / `GitError` 向上传播。
>
> **2026-07-01 更新 2**：`try_read_pkt_line` 的 pkt-line parser 错误模型继续收敛。新增回归测试覆盖 reserved length `0003`、length 小于 header（如 `0002want`）的精确错误诊断与不消费输入行为，以及非十六进制 header、短 header、payload 不完整的既有测试；由 `try_read_pkt_line_rejects_reserved_length_3`、`try_read_pkt_line_rejects_length_smaller_than_header`、`try_read_pkt_line_rejects_non_hex_header`、`try_read_pkt_line_rejects_incomplete_header_without_consuming`、`try_read_pkt_line_rejects_incomplete_payload` 共同锁定。
>
> **2026-07-01 更新 3**：真实 Git CLI CI 矩阵确认覆盖 HTTP 只读路径（ls-remote、clone、fetch、protocol v2 fetch/ls-remote、shallow clone、blob:none partial clone）。**2026-07-01 更新 4**：HTTP **CL push**（客户端 `HEAD:refs/heads/<name>` → 服务端 `refs/cl/*`，**不**新增公开分支）已进入 CI 必跑 gate；workflow 在 smoke DB 中生成并 mask 一次性 `access_token`，通过 Basic Auth URL 运行 `scripts/git_protocol_smoke.sh` 的 `MEGA2_GIT_SMOKE_PUSH=1` 分支。为支持该路径，monorepo receive-pack 会在 old-id 为零但新提交带 parent 时以首个 parent 作为 CL base；孤儿提交初始化仍拒绝。
>
> **2026-08-03 更新**：Monorepo 公开分支仅 `main`；`Monorepo::update_refs` **拒绝** Git 客户端创建、更新和删除 Tag（Tag 通过 HTTP API 管理）。smoke 将原「push and delete tag」改为 **reject Git-client tag push**，并将 branch smoke 改名为 CL push，断言 `refs/heads/*` 集合不变。
>
> **2026-08-29 更新**：Monorepo receive-pack 放开**多 commit 链式 push**（plan-20260827 MC-06，唯一用户可见行为变化）：单次 push 允许 2..=250 个 commit 的线性链（上限为 CL 累计范围口径，ADR-MC-07），新分支 push 的 CL base 按 pack 成员关系沿首父链反走到 fork point（取代单 commit 时代的「取 tip 首个 parent」近似）；含 merge commit 的链、断链/环、tip 与 ref 更新目标不符、累计超限，一律在 finalize 写 ref/CL 之前 fail-closed（MC-03 校验器正式接通主路径）；单次 receive-pack 多于一条非删除 branch 命令时整体拒绝并引导分次 push（ADR-MC-04 收紧生效，删除命令不受影响）。unpack 不再拒绝第二个 commit entry，也不再要求 pack commit 等于 `new_id`（改由 finalize 的「pack 必须携带 ref 更新目标」检查等价兜底）。端到端由 `tests/integration_git_cli.rs` 的 `multicommit` 用例族锁定；trunk merge 语义不变（ADR-MC-01）。
>
> **2026-08-29 更新 2（MC-06 评审硬化）**：fork point 反走只沿「本 push **新引入**」的 commit 前进（unpack 时已存在于服务端的祖先即便被 pack 冗余携带也不构成链），反走与校验共享 250 上界；校验器增量段改为内存校验（复用 resolve 的反走结果，不再重复读库），历史段仍仅计数；commit 绑定从 unpack 阶段后移到 finalize 成功之后且仅覆盖已接受链——被拒 push 不再 upsert `commit_auths`；混合「删除 + 单条更新」的 receive-pack 明确放行（ADR-MC-04 的删除豁免由 e2e 锁定）。
>
> **2026-09-12 更新（plan-20260901 FC-08）**：receive-pack 对齐 Mega 的 **per-command report-status**：pack-less 非删除命令校验已存 object；monorepo tag 与 `refs/heads/main` 删除在持久化前 `ng`；多余非删除 branch 标 `ng` 而第一条仍可 finalize（不再整包拒绝）；tag-only 不 `finalize_receive_pack`。CL delete-only 保持允许。
>
> **2026-09-12 更新（plan-20260901 FC-09）**：ref 删除按客户端 advertised `old_id` 做 compare-and-swap；不匹配时不删除已被并发更新的 ref，report-status 为 `moved since advertisement`。默认分支删除仍由 FC-08 前置规则拒绝。
>
> **2026-09-12 更新（plan-20260901 FC-10）**：import/monorepo filepath 写入改为收集 `(blob_id, path)` 后按 repo/domain 一次 CASE UPDATE（empty 为 no-op，duplicate 保留最后一次 path）。成功导入的路径语义不变。
>
> **2026-08-29 更新 3（MC-06 R2 粘性收口）**：拒绝规则改为只基于 pack **内容**（presence 集）——resolve 沿 tip 首父路径做 presence 界定的走查（新引入成员走 250 语义上界，冗余携带的已知祖先走独立卫生上界），凡不在路径上的 pack commit 一律判 junk 拒绝，与瞬时 newness 无关；无新引入的 push 不再退化为 `[tip]` 链，而是把整段内容链交给校验器重验。净效果：**同一被拒 pack 原样重试必被同样拒绝**（junk / 链中 merge / 超长 / 累计超限全部粘性），且 ref/CL/`commit_auths` 零变化；已接受 push 的幂等重试在真实客户端下天然走空 pack no-op 分支不受影响。绑定消费收窄为 `ordered ∩ 新引入`。

> **2026-07-01 更新 5**：LFS opt-in smoke 已对齐 monorepo push 语义。`lfs_smoke_http` 不再从孤儿仓库初始化并推送不可接受的 root commit，而是先 clone 远端默认分支、创建普通子提交、用 non-delta pack 推送，再通过 `refs/cl/*` 差异定位 mega2 实际创建的 CL ref；clone/fetch/LFS pull 与清理都针对该 CL ref 执行。**2026-07-01 更新 6**：CI workflow 现在安装 `git-lfs` 并默认启用 `MEGA2_GIT_SMOKE_LFS=1`，LFS push/clone/pull/locks-list round-trip 进入 `Git Protocol Smoke` 必跑 gate。
>
> **2026-07-01 更新 7**：首轮 LFS CI gate 暴露 Git LFS 对 monorepo 根路径 remote `http://host/` 的默认 discovery 派生问题：客户端会拼出 `http://host.git/info/lfs` 并导致端口解析失败。`lfs_smoke_http` 现在对源仓库和验证 clone 显式配置 `lfs.url=<remote>/info/lfs`，并关闭 locks verify 探测，避免根路径 URL 被 Git LFS 自动追加 `.git`。
>
> **2026-07-01 更新 8**：第二轮 LFS CI gate 暴露普通 `git clone` 在临时 LFS push 后会先拉取 remote 全量 refs，并可在 upload-pack 阶段因临时 CL ref 状态返回 HTTP 400。`lfs_smoke_http` 的验证端现在改为空仓库 `git init`，只 fetch 本次创建的 `refs/cl/*` 到本地 `lfs-smoke` 分支，再执行 checkout、`git lfs pull` 和 locks-list；CI 验证聚焦在目标 CL ref 的 LFS round-trip，不再依赖全量 clone。

> **2026-09-09 更新（TP-17 / ADR-TP-18）**：monorepo `finalize_receive_pack` 按 `[monorepo].push_policy` 条件化。`review` 仍走 `persist_mono_branch_cl_mega_refs_transaction` + CL post-push 管线（写 `refs/cl/*`）。`trunk` 跳过 CL 落地，以 `kind=push` 入队并执行 B3：N=1 客户端 commit 原样落 `main@P`，N>1 squash（sideband 返回合成 id 与对齐命令 `git fetch && git reset --hard origin/main`）；删除类 branch 命令在 finalize 以 B0 正文拒绝。trunk 形态 `refs_with_head_hash` 过滤 `is_cl` 行（档案 CL refs 不 advertise）。`push_queue.requester` 取协议身份（token 名 / `none` 为 NULL），不用 commit author。

> **2026-09-10 更新（plan-20260908 SP-01）：** storage-only SSH 读与 HTTP 共用 `anonymous_access`：仅 `push_auth.is_some() && anonymous_access=true` 时 `auth_none` Accept。review 即使匿名开也不 Accept `auth_none`（保护公钥推送）。storage-only `auth_publickey` 早拒。进程 IT：`integration_git_ssh_trunk_none_anon_on_clone`、`integration_git_ssh_trunk_none_anon_off_clone_fail`、`integration_git_ssh_trunk_token_anon_on_clone`。
>
> **2026-09-16 更新（plan-20260912 / WH-05）**：storage-only LFS basic 上传在**本次请求实际 `put_stream` 成功**后发一次 `lfs.object.uploaded` 出站事件（仅 oid/size/`transfer="basic"` 元数据，scope 三个值全 null，只发给 `include_unscoped_lfs=true` 的静态 target；契约见 [`storage-events.md`](storage-events.md)）。明确不发的路径：batch 建行、exists 命中 no-op、hash/size 校验失败、presigned 直传（对象存储 `signed_url` 绕过本机 upload handler，无完成回调，覆盖缺口登记为 DEFER-WH-01）。exists→put 非原子，并发上传可重复通知，不承诺首次写 exactly-once。进程 IT：`integration_git_lfs_storage_events_basic_upload`（basic 一发）/ `integration_git_lfs_storage_events_presigned_gap`（RustFS 直传零事件）。
>
> **2026-09-12 更新（plan-20260907 B3-04）：** `monorepo.object_format` 为 `sha256` / `blake3` 时，normal Git service 会 advertise `object-format={kind}`，pack 编解码走 `*_with_hash_kind`。这是 **git-internal / Libra extension**，**不是** 标准 Git BLAKE3 互通（永久非目标，DEFER-B3-01）。跨 kind / 错宽度 ID 与 pack trailer 错配 fail-closed。

> **2026-09-10 更新（plan-20260908 SP-02）：** `push_auth=token` 时 SSH `auth_password` 复用 `lookup_push_token`（username 不参与判定，身份为 token `name`）。客户端用 `SSH_ASKPASS`（password 不进 tracing、不进 `GIT_SSH_COMMAND`）。`push_auth=none` 与 review（省略）均 Reject password。进程 IT：`integration_git_ssh_trunk_token_anon_off_clone_fail`、`integration_git_ssh_trunk_token_anon_off_password_clone` / `_fetch` / `_pull`、`integration_git_ssh_trunk_token_push_receive_pack_disabled`、`integration_git_ssh_review_pubkey_anon_off_clone`、`integration_git_ssh_review_pubkey_anon_on_push`。

> **2026-09-11 更新（plan-20260906 SO-19..SO-25 / SO-04）：** storage-only SSH 黑盒为 `scripts/git_protocol_smoke_storage_only.sh`：只读矩阵（`SSH ls-remote` … `SSH protocol v2 blob:none clone`）+ **`SSH reject receive-pack`**（断言稳定子串 `SSH receive-pack is disabled`；未设 `MEGA2_SSH_REPO_URL` 时 SKIP）。默认 `GIT_SSH_COMMAND` 使用 `StrictHostKeyChecking=accept-new` + workdir known_hosts；`anonymous_access=true` 下 `auth_none`。对照 review smoke 与 plan-20260908。

1. **HTTP 和 SSH 双协议支持已就位**。`contract::git_protocol/http.rs` 和 `contract::git_protocol/ssh.rs` 分别实现两个协议入口，共用 `SmartSession` 和 `src/ceres/protocol/smart.rs` 的 smart protocol 实现。

2. **基础 fetch/push/clone 可工作**。当前能支持标准 Git 客户端的基本 clone、fetch、push 操作，但多处使用 `unwrap()` 和缺乏边界检查。

3. **pkt-line 解析与 receive-pack 分流已完成首批止血。** `read_pkt_line` 的可失败版本已落地，HTTP/SSH receive-pack 已按 flush-pkt 分割 commands 与 pack bytes，不再搜索 `PACK` magic；delete-only push（全 delete）已支持（跳过 unpack）；残余风险是当前实现仍缓冲完整 body / channel 数据，尚未实现真正 streaming pkt-line reader。

4. **认证上下文已统一（HTTP/SSH）。** HTTP receive-pack 需要 Bearer/Basic token，upload-pack 无认证；SSH publickey 认证成功后保存 username 并传入 `SmartSession`，commit binding 绑定到 authenticated actor。receive-pack 尚未做 repo/path 级 push 权限校验。

5. **Capability advertise 已完成保守收敛与 truth table 覆盖**。receive-pack 重新 advertise 已由 HTTP CL delete smoke 覆盖的 `delete-refs`，但不再 advertise 未验证的 atomic、report-status-v2、quiet、no-thin；upload-pack当前发布版不advertise include-tag；MTR13候选拟仅为支持该语义的ImportRepo按handler通告，Monorepo/ViewRepo保持不通告，须自身C/D后生效；v2 不再 advertise 未 act-on 的 `server-option`。`side-band-64k`/`ofs-delta` 已补 advertise/parse 单测（ofs-delta pack decode 委托 `git-internal`），`object-format` 落地 SHA-1 默认策略；**（2026-06-30）真实 Git CLI 兼容性矩阵已通过 `.github/workflows/git-protocol-smoke.yml` 在 CI 中自动化执行**。用户可见的分支与 Tag 规则见 [`../user-guide.zh.md`](../user-guide.zh.md)。

6. **SSH 多 channel 状态已隔离，并已支持 protocol v2。** `SshServer` 按 `ChannelId` 保存独立 `GitSshChannelState`；SSH client 通过 `GIT_PROTOCOL=version=2` 请求 v2 时，server 返回 v2 capability advertisement，并在 upload-pack data 阶段分发 `ls-refs` / `fetch` command。

## 当前实现状态速览表

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
|-----------|--------|-------------|
| HTTP GET /info/refs | 已实现（含 protocol v2 advertisement） | query 已要求 exactly one `service=...`；缺失、重复、非法或额外参数均返回 `ProtocolError::InvalidInput`；`Git-Protocol: version=2` 会返回 v2 capabilities；**真实 Git CLI 兼容性矩阵已通过 CI smoke gate 覆盖（2026-06-30）**。 |
| HTTP POST upload-pack | 已实现（含 shallow / v2 fetch / blob:none） | 一次性读取 request body 到内存；pkt-line 与 `want`/`have` malformed input 已返回协议错误；protocol v1 支持 `deepen`/`deepen-relative`，v2 支持 `ls-refs`、`fetch`、`deepen`、`filter blob:none`；仍不支持 streaming request parser。 |
| HTTP POST receive-pack | 已实现（CL push + delete-only / pack-less 与 HTTP smoke 已纳入 CI） | command pkt-line malformed input、空命令列表与无效 `PACK` magic 已返回协议错误；flush 后空 payload 为 pack-less（不再要求非删除必须带 pack）；commands / pack 已按 flush-pkt 分割；delete-only 与 pack-less create/update 跳过 unpack；HTTP **CL push** 与 **Git-client tag 拒绝** 已进入 Git CLI smoke CI gate；**多 commit 线性链 push 已放开（2..=250 累计口径）**；FC-08 起单次多条非删除 branch 为 mixed report-status（多余 `ng`，第一条可 finalize），而不是整包拒绝；仍需 streaming parser。 |
| SSH git-upload-pack | 已实现（per-channel state + protocol v2） | exec command 已走独立 parser，支持基础 shell quoting、包含空格的路径和严格命令白名单；upload-pack 初始响应已按 bytes 发送；`SshServer` 已按 `ChannelId` 隔离 `SmartSession` 与 receive-pack 缓冲区；`GIT_PROTOCOL=version=2` 可启用 v2 `ls-refs` / `fetch`。 |
| SSH git-receive-pack | 已实现（per-channel state） | 与 HTTP 共用 flush-pkt 分割逻辑，不再搜索 `PACK`；每个 SSH channel 拥有独立的 receive-pack 缓冲区，多 channel 不再共享状态。 |
| SSH git-lfs-authenticate / transfer | 已实现 hybrid；pure SSH transfer 明确 unsupported | `git-lfs-authenticate` 支持 hybrid 模式，返回 HTTP LFS URL；`git-lfs-authenticate` / `git-lfs-transfer` 均要求 operation 为 `upload` 或 `download`；`git-lfs-transfer` 通过 stderr extended-data 返回明确 unsupported 错误 + channel failure，不再输出普通占位文本。 |
| 权限与认证 | 已统一（读写策略化） | HTTP receive-pack 有 Bearer/Basic token 认证；review SSH 走 publickey，storage-only `push_auth=token` 时 SSH `auth_password` 命中 `lookup_push_token` 后将 token `name` 写入 `authenticated_user`。HTTP/SSH commit binding 均绑定到 authenticated actor（`set_authenticated_user`）。upload-pack 匿名访问已策略化：`check_upload_pack_access` 按 `git.anonymous_access`（默认 `true`）放行匿名 clone/fetch，关闭时无 token 返回 `Forbidden`（3 个单测覆盖），HTTP（`http.rs:53/217`）与 SSH（`ssh.rs:149`）共用。receive-pack 通过 `check_push_permission` 走 Cedar `pushRepo` 授权：未认证 push 直接 `Forbidden`（不进入 unpack），已认证时按 `state.entity_store` 策略裁决（策略库为空则放行），HTTP（`http.rs:64/355`）与 SSH（`ssh.rs:140`）共用。更细粒度 repo/path ACL 与基于策略的 push-deny 协议层单测待后续。 |
| Capability advertise | 保守收敛 + truth table 已建立 | receive-pack 仅 advertise `report-status` + common 能力；upload-pack当前发布版移除 `include-tag`，MTR13候选仅ImportRepo按handler通告（待C/D），其余handler保持移除；v2 移除 `server-option`。`side-band-64k`/`ofs-delta` 已覆盖 advertise/parse（ofs-delta pack decode 委托 `git-internal`）；`object-format` 落地 SHA-1 默认策略；`RepoHandler::supports_shallow_fetch`/`supports_filtered_fetch` 门控非 Monorepo handler。**（2026-06-30）真实 Git CLI 兼容性矩阵已通过 CI smoke gate 覆盖**。 |
| 错误处理 | 首批止血 | `info/refs` service 参数、smart pkt-line malformed input、HTTP upload/receive request body stream 错误、malformed SSH exec 与 import repo handler 的 repo path/DB lookup 已改为协议错误/channel failure；SSH `data`/`handle_upload_pack`/`handle_receive_pack` 中的 `smart_protocol.unwrap()`、protocol error `.unwrap()`、`session.data().unwrap()`、`git-lfs-authenticate` response serialization `.unwrap()` 和 `auth_publickey` DB 查询 `.unwrap()` 已改为可诊断错误/best-effort 发送（2026-06-23/24）；**2026-06-28 更新**：`Repo::new` 的非 UTF-8 path/file_name `unwrap()` 已改为 `ProtocolError::InvalidInput`，`SmartSession::git_upload_pack` 中 `full_pack`/`incremental_pack` 的 `unwrap()` 已映射为协议错误；**2026-06-28 更新 2**：HTTP Git 路由已抽出 `GitProtocolPath` parser，移除内联 `.git` 替换与 404 `unwrap()`；**2026-06-28 更新 3**：`contract/git_protocol/http.rs` 中 response builder、`HeaderValue::from_str`、upload-pack sideband `read_buf` 的 `unwrap()` 已收敛；**2026-06-28 更新 4**：`contract/git_protocol/ssh.rs` 中 LFS `Duration::try_seconds(...).unwrap()` 与 `channel_eof` 的 `expect("state just found")` 已移除。Protocol 当前范围内剩余 `unwrap()` 已基本收敛；vault/legacy 路径与数据迁移工具不在本次范围。 |

## 硬约束与不可违反的原则

1. **协议正确性优先**：Git smart protocol 是二进制协议，所有 payload 必须按 bytes 处理，不能假设 UTF-8 或文本。

2. **客户端不能导致崩溃**：任何 malformed client input（非法 query、脆弱 exec、malformed pkt-line）都必须返回协议错误，不能 panic。

3. **Capability 诚实**：只 advertise 已实现且有测试的能力。未实现的能力如 atomic、report-status-v2 应先完齐或从 advertise 中移除。

4. **权限强制**：auth context 必须统一（HTTP Bearer/Basic + SSH key），并在 upload-pack/receive-pack 入口前完成。push 必须验证用户和权限。

5. **pkt-line 协议边界**：receive-pack 的 command 和 pack 分界必须由 flush-pkt 决定，不能依赖 magic bytes 搜索。

6. **兼容性声明**：已实现的现代能力（protocol v2、shallow clone、`filter blob:none`）必须明确范围；未实现的功能（tree filters、完整 promisor remote、pure SSH LFS）必须明确文档化，而不是静默失败。

## 现状 vs 目标对比

| 维度 | 当前状态 | 目标状态 | 关键差距 |
|-----|--------|--------|--------|
| 错误处理 | 多处 panic | 协议错误或断开，不能 panic | 需要 Result 化所有输入解析 |
| pkt-line 实现 | `unwrap()` panic 散落 | 定义 `PktLine` enum 和 streaming parser | 需重构 parser 返回 Result |
| receive-pack 分流 | 已按 flush-pkt 分割，仍缓冲完整 body/channel | streaming pkt-line reader + delete-only push 验证 | 中等 |
| SSH exec 解析 | 脆弱（空格、拼接错） | 严格 parser，支持引号和转义 | 需独立 parser 函数和单测 |
| SSH 多 channel | 全局 session 状态 | per-channel state dictionary | 已引入 `GitSshChannelState`（2026-06-28） |
| 认证统一 | ✅ HTTP/SSH 共用 `check_upload_pack_access` / `check_push_permission` | 统一入口鉴权 + 匿名策略 + push 授权 | upload-pack 匿名可配置、receive-pack Cedar `pushRepo` 授权已落地；更细 repo/path ACL 与 push-deny 协议层单测待后续 |
| Capability 诚实 | advertise 多于实现 | 仅 advertise 已实现能力 | 需 capability truth table |
| 测试矩阵 | 无 | 真实 Git CLI smoke test | 需建立兼容性测试脚本 |

## 前置依赖矩阵（2026-06-14）

本文档与其他改进计划的依赖关系：

| Git Protocol 的工作 | 对其他模块的依赖 | 依赖类型 | 关键同步点 |
|------------|-----------|--------|---------|
| **阶段 0：兼容性基线** | general.md | 框架 | 必须遵守结构规范 |
| **阶段 1：panic 止血** | general.md | 框架 | 错误模型与 config 的 error.rs 对齐 |
| **阶段 2-3：pkt-line/capability** | general.md | 框架 | 与整体错误处理框架一致 |
| **阶段 4：auth 统一** | config.md | 前置 | 需先完成 config 阶段 2 的 CLI LoadMode |
| **阶段 5-6：SSH/LFS 完善** | integration.md | 并行 | 集成测试可同步验证改进 |

**注**：阶段 1-3 相对独立，可先完成基础止血。阶段 4 需与 config 协同，推荐在 config 阶段 2 之后。

## 风险与约束

- **二进制协议处理**：当前多处假设 UTF-8，需要完整的 bytes 化改造。如果改造不彻底，会在处理特殊 pack 数据或 binary diff 时出现隐蔽问题。

- **receive-pack 事务边界**：当前 atomic 的声称不被测试覆盖，并发 push 的锁粒度不明确。如果不完整地实现事务，可能导致数据不一致。

- **SSH 多 channel 状态**：state 在 handler 上全局保存，虽然 clone 降低共享风险，但一个 connection 内多个 channel 仍可能互相影响。

- **Capability 误导**：advertise 未实现的 capability 会让 Git 客户端进入不支持的代码路径，产生隐蔽失败。必须建立 truth table 防止此类问题。

- **兼容性测试缺失**：没有真实 Git CLI 的测试矩阵，无法防止 panic 或协议误差的回归。

## 多维评估表

| 维度 | 评估结论 |
|-----|--------|
| **合理性** | **高（8.5/10）**。当前代码框架完整，兼容性问题识别准确。阶段划分合理，从基础止血到兼容性扩展的路线清晰。 |
| **可行性** | **中高（7.5/10）**。基础 panic 止血相对容易（Result 化输入解析）；pkt-line 和 receive-pack 分流需要较深的协议理解；SSH auth 统一需与 config 协同。 |
| **完整性** | **中高（7.5/10）**。6 个阶段覆盖主要问题，且已补齐 shallow clone、protocol v2 `ls-refs` / `fetch`、`filter blob:none` 等现代 Git 基础能力。仍缺真实 Git CLI 矩阵、streaming pkt-line reader、tree filters 和完整 promisor remote 语义。 |
| **安全性** | **中（7/10）**。panic 止血直接提升安全性；auth 统一防止权限泄露。但 per-channel state 和事务边界的改进是长期工作。 |
| **可维护性** | **中（7/10）**。首批已移除 magic bytes 搜索和部分 scattered panic；SSH channel state 已改为 per-channel 字典。剩余 `unwrap()` 和 streaming parser 缺口仍是维护陷阱。改进后代码应更易维护，但短期工作量较大。 |

## 小结

Git Protocol 是 mega2 的核心功能，但当前实现在错误处理、兼容性和细节上存在多个缺陷。建议优先完成阶段 0-1 的兼容性基线建立和 panic 止血，确保 malformed input 不导致崩溃。再依次完成阶段 2-3 的 pkt-line 和 receive-pack 改造，确保协议边界正确。阶段 4 的 auth 统一可与 config 模块协同完成，阶段 5-6 为长期改进。

## 预期收益

- **崩溃修复**：消除 panic 风险，malformed input 返回协议错误而非连接中断
- **兼容性提升**：标准 Git 客户端在各场景下（clone、fetch、push、tags、delete）更稳定可靠
- **可观测性改进**：清晰的错误模型和兼容性测试矩阵便于快速诊断和防止回归
- **维护成本降低**：代码结构改进（streaming parser、per-channel state）降低后续改造成本
- **扩展性增强**：shallow clone、protocol v2 和 `filter blob:none` 已落地，为后续 tree filters、promisor remote 和 streaming parser 继续预留设计空间

## 范围

本文覆盖：

- Git smart HTTP：`GET /info/refs?service=...`、`POST /git-upload-pack`、`POST /git-receive-pack`。
- Git SSH：`git-upload-pack '<repo>'`、`git-receive-pack '<repo>'`、`git-lfs-authenticate`。
- Git smart protocol 的 pkt-line、capability、side-band、upload-pack、receive-pack。
- Git LFS 与 SSH hybrid LFS discovery 的兼容性。
- 与标准 `git clone`、`git fetch`、`git push`、`git lfs` 客户端交互相关的认证、错误处理和测试建议。

不覆盖普通 REST API、Web UI、内部 monorepo API 或对象存储协议。

## 当前实现概览

### HTTP 入口

HTTP 服务在 `src/server/http_server.rs` 中注册 Git 协议入口：

```text
/{*path}
  -> GET  */info/refs
  -> POST */git-upload-pack
  -> POST */git-receive-pack
```

核心分发函数是 `handle_smart_protocol`：

- `GET .../info/refs` 调用 `contract::git_protocol::http::git_info_refs`。
- `POST .../git-upload-pack` 调用 `contract::git_protocol::http::git_upload_pack`。
- `POST .../git-receive-pack` 调用 `contract::git_protocol::http::git_receive_pack`。
- 其他路径返回 404 `Operation not supported`。

HTTP Git 协议处理代码位于 `src/contract/git_protocol/http.rs`。该模块负责：

- 解析 `InfoRefsParams.service`。
- 创建 `SmartSession`。
- 读取 request body。
- 调用共享 smart protocol 实现。
- 设置 Git smart HTTP 所需的 `Content-Type` 和 `Cache-Control`。
- 对 receive-pack 执行 Bearer token 或 Basic Auth token 认证。

### SSH 入口

SSH 服务入口位于 `src/server/ssh_server.rs` 和 `src/contract/git_protocol/ssh.rs`。

`src/server/ssh_server.rs` 负责：

- 加载或生成 SSH host key，并保存到 vault 的 `ssh_server_key`。
- 构造 `russh::server::Config`。
- 构造 `ProtocolApiState`。
- 运行 `SshServer`。

`src/contract/git_protocol/ssh.rs` 负责：

- 处理 `exec_request`。
- 支持 `git-upload-pack` 和 `git-receive-pack`。
- 支持 `git-lfs-authenticate` hybrid LFS discovery。
- 对 `git-lfs-transfer` 返回明确 unsupported failure。
- SSH 认证：review（省略 `push_auth`）用 UserStorage publickey，**不**成功放行 `auth_none`；storage-only 在 `anonymous_access=true` 时 `auth_none` Accept，`auth_publickey` 早拒（不查 UserStorage）。`ssh_receive_pack=false` 只关 receive-pack，不关 upload-pack。

SSH 和 HTTP 最终共用 `SmartSession` 与 `src/ceres/protocol/smart.rs` 中的 smart protocol 实现。

### SmartSession 与 repo handler

`SmartSession` 定义在 `src/ceres/protocol/mod.rs`，核心字段包括：

- `repo_path`
- `service_type`：`UploadPack` 或 `ReceivePack`
- `transport_protocol`：`Http` 或 `Ssh`
- `auth`
- `capabilities`
- `view`：`Option<ResolvedView>`，视图 URL 经第 3 层解析后为 `Some`，其余会话为 `None`；`new` / `from_state` 签名不变。

`repo_handler_with_commands` 先看 `view`，再根据 repo path 选择 repo handler：

- 如果 `view` 为 `Some`，使用 `ViewRepo`：用解析时取得的配置快照构造，在读取 `state.storage.config()` 前返回，不看 repo path。
- 如果 path 位于 `config.monorepo.import_dir` 下，使用 `ImportRepo`。
- 否则使用 `Monorepo`。

`ImportRepo` 和 `Monorepo` 都实现 `RepoHandler`，提供 pack 生成、pack 解码、ref 更新、receive-pack finalize 等能力。

### Smart protocol 实现

核心实现位于 `src/ceres/protocol/smart.rs`：

- `git_info_refs`：构造 ref advertisement。
- `git_upload_pack`：解析 `want`、`have`、`done`，返回 ACK/NAK 与 pack stream。
- `parse_receive_pack_commands`：解析 receive-pack ref update commands。
- `git_receive_pack_stream`：解码 pack stream、保存对象、更新 refs、返回 report-status。
- `build_side_band_format`：按 side-band / side-band-64k 包装 pack 数据。
- `read_pkt_line` / `add_pkt_line_string`：pkt-line 编解码辅助。

当前 advertise 的 capability：

```text
upload-pack:
  multi_ack_detailed no-done side-band-64k ofs-delta shallow agent=mega/0.1.0

receive-pack:
  report-status side-band-64k ofs-delta agent=mega/0.1.0

protocol v2 upload-pack:
  agent=mega/0.1.0 ls-refs fetch=shallow filter object-format=sha1
```

`Capability` enum 当前只解析部分 capability：

```text
multi_ack
multi_ack_detailed
no-done
side-band
side-band-64k
report-status
report-status-v2
ofs-delta
deepen-since
deepen-not
shallow
```

## 当前兼容能力

### 已支持的 HTTP smart protocol 基础链路

当前实现已经覆盖 smart HTTP 的三类核心请求：

- ref discovery：`GET /repo.git/info/refs?service=git-upload-pack`。
- fetch/clone：`POST /repo.git/git-upload-pack`。
- push：`POST /repo.git/git-receive-pack`。

HTTP response 已设置：

- `Content-Type: application/x-git-upload-pack-advertisement`
- `Content-Type: application/x-git-receive-pack-advertisement`
- `Content-Type: application/x-git-upload-pack-result`
- `Content-Type: application/x-git-receive-pack-result`
- `Cache-Control: no-cache, max-age=0, must-revalidate`

### 已支持的 SSH smart protocol 基础链路

当前 SSH 实现支持标准 Git SSH exec 命令：

```text
git-upload-pack '<repo>.git'
git-receive-pack '<repo>.git'
```

认证采用 SSH public key fingerprint 匹配数据库中的用户 SSH key。认证成功后，`git-upload-pack` 和 `git-receive-pack` 复用 `SmartSession`。

### 已支持基础 push/fetch 数据流

fetch 侧：

- 支持 `want`、`have`、`done`。
- 支持无 `have` 时 full pack。
- 支持有 `have` 时 incremental pack。
- 支持 `multi_ack_detailed` 下的 `ACK ... common` 和 `ACK ... ready`。
- 支持 side-band-64k 包装 pack data。
- 支持 `deepen` / `deepen-relative` shallow fetch，并 advertise `shallow` capability。
- 支持 protocol v2 `ls-refs` / `fetch`（HTTP 通过 `Git-Protocol: version=2`，SSH 通过 `GIT_PROTOCOL=version=2`）。
- 支持 protocol v2 partial clone `filter blob:none`；tree filter 仍未承诺完整语义。

push 侧：

- 支持 receive-pack command 解析。
- 支持 pack stream 解码。
- 支持对象保存。
- 支持 refs 更新。
- 支持 `report-status` 风格的 `unpack ok` 与 per-ref status。
- 支持 tag 和 branch 的基本更新路径。

### 已支持 Git LFS HTTP discovery 兼容路径

LFS router 同时暴露：

- `/api/v1/lfs/...`
- `/info/lfs/...`

HTTP server 还包含 `rewrite_lfs_request_uri`，用于把 repo path 下的 `/info/lfs/...` 重写到 LFS runtime router，并在改写前把仓库前缀写入 `LfsRepoContext`。SSH 下 `git-lfs-authenticate` 会返回 HTTP LFS URL，让 Git LFS 客户端走 hybrid 模式。

开启 `fastcdc` Cargo feature 时，同一 LFS mount 额外提供 Libra FastCDC Media API：`<repo>.git/info/lfs/libra/media/v1/...`（不是无 `libra/` 前缀的 `media/v1`，也不是 Git LFS 扩展）。每条 Media 路由（含 capabilities）都要求 Bearer `AccessTokenUser`，不复用 objects 传输 URL 的「batch 后可不再认证」例外。关闭 feature 时这些路由不存在（404），既有 LFS batch/objects/locks 行为不变。协议细节见 [`fastcdc-media.md`](fastcdc-media.md)。

## 主要兼容性问题

### HTTP info/refs 参数校验（首批已止血）

`handle_smart_protocol` 当前通过 `parse_info_refs_params` 解析并校验 query，反序列化失败、缺失/重复/非法 `service` 或存在额外 query 参数都返回 `ProtocolError::InvalidInput`；`contract::git_protocol::http::git_info_refs` 仍保留同类防御性校验。旧实现曾直接：

```rust
let service_name = params.service.unwrap();
let service_type = service_name.parse::<ServiceType>().unwrap();
let params: InfoRefsParams = serde_urlencoded::from_str(query_str).unwrap();
```

已处理风险：

- query 反序列化错误会返回 400，不再 panic。
- 缺少 `service` 会返回 400，不再 panic。
- 重复 `service` 或额外 query 参数会返回 400。
- 非法 service 会返回 400，不再 panic。

仍待后续：

- 更完整 smart HTTP query 兼容性矩阵和真实 Git CLI 覆盖。
- 按真实 Git CLI 矩阵确认是否需要对非标准客户端提供兼容模式。

建议：

- 缺失、重复、额外参数或非法 `service` 返回 400。
- 不支持的 service 返回 403 或 400，并输出 Git 客户端可读错误。
- 对额外 query 参数做显式策略：首版已选择拒绝；如未来需要兼容非标准客户端，应先补矩阵并记录偏离规范。
- 所有解析失败都返回 `ProtocolError`，不能 panic。

### HTTP upload-pack 会一次性读取完整请求体

`git_upload_pack` 当前通过 `try_fold(BytesMut::new())` 把整个 request body 读入内存。fetch 请求通常较小，但 protocol v0 negotiation 在复杂仓库和大量 `have` 场景下仍可能较大。body stream 读取错误已统一映射为 `ProtocolError::InvalidInput`，不再 panic。

建议：

- ✅ 已增加 body size 上限（`GIT_HTTP_MAX_BODY_BYTES = 512 MiB`，`collect_body_data` 拒绝超限 body，2026-06-28）。
- 中期将 upload-pack negotiation 改为 streaming pkt-line reader，而不是一次性聚合 body。
- 为 `read_pkt_line` 增加可诊断错误，避免 malformed pkt-line panic。

### HTTP / SSH receive-pack 分割 commands 和 pack 数据

早期 `git_receive_pack` 在 chunk 中搜索字节序列 `PACK`：

```rust
if let Some(pos) = search_subsequence(&chunk, b"PACK") {
    chunk_buffer.extend_from_slice(&chunk[0..pos]);
    let commands = pack_protocol.parse_receive_pack_commands(Bytes::copy_from_slice(&chunk_buffer));
    let left_chunk_bytes = Bytes::copy_from_slice(&chunk[pos..]);
    ...
}
```

这一路径已在 2026-06-23 的首批止血中替换为 `SmartSession::split_receive_pack_request`：

- HTTP receive-pack 先聚合 request body，再按 pkt-line command list 的 flush-pkt 分割 commands 与 pack bytes。
- SSH receive-pack 对已缓冲的 channel 数据使用同一 splitter。
- capability 或 command payload 中出现 `PACK` 不再影响分割。
- 缺失 flush-pkt 会返回 `ProtocolError::InvalidInput`。
- 纯 delete refs 的无 pack receive-pack 请求已支持，splitter 会在 flush-pkt
  后返回空 pack payload，并由 `is_delete_only_push` 跳过 unpack。
- 非 delete receive-pack 请求在 flush-pkt 后缺失 pack payload 会返回
  `ProtocolError::InvalidInput`。
- 非 delete receive-pack 请求的 pack payload 不以 `PACK` magic 开头时会返回
  `ProtocolError::InvalidInput`。
- flush-pkt 前没有任何 command pkt-line（空命令列表）的 receive-pack 请求会返回
  `ProtocolError::InvalidInput`，避免无命令请求进入 ref 处理。

仍待后续处理：

- 将当前完整 body / channel 数据缓冲改为 streaming pkt-line reader。
- HTTP branch/tag push+delete 与 LFS round-trip 已纳入真实 Git CLI CI 矩阵；SSH push/delete 仍为手动 opt-in。

### SSH exec command 解析过于脆弱

`exec_request` 早期使用：

```rust
let command: Vec<_> = data.split(' ').collect();
let path = command[1];
let path = path.replace(".git", "").replace('\'', "");
let service_type = ServiceType::from_str(command[0]).unwrap_or(ServiceType::UploadPack);
```

这一路径已在 2026-06-23 的首批止血中替换为独立 parser。当前 parser 已覆盖以下风险：

- 空命令或缺 path 会 panic。
- 路径包含空格会被错误切分。
- `.replace(".git", "")` 会删除路径中所有 `.git`，而不是只去掉末尾 `.git`。
- 不支持单引号、双引号和反斜杠转义。
- 非法 command 默认变成 `UploadPack`，可能导致错误行为。

仍待后续处理：

- 更完整的 shell 兼容性（如 `--`、复杂转义规则）如果标准客户端需要，再按兼容性测试补充。
- ✅ 将已解析的 exec state 与 SSH channel 绑定，避免一个 connection 内多个 channel 共享 `smart_protocol` / `data_combined`（2026-06-28 已实现）。

### SSH session 状态与 channel 绑定

`SshServer` 原在 handler 上保存连接级 `smart_protocol: Option<SmartSession>` 与 `data_combined: BytesMut`，一个 SSH connection 内多个 channel 会共享这些状态。2026-06-28 已改为：

- `SshServer` 维护 `channels: HashMap<ChannelId, GitSshChannelState>`。
- `GitSshChannelState` 每个 channel 单独保存 `SmartSession`、receive-pack buffer、service type。
- `data` 只读写当前 `ChannelId` 的状态。
- `channel_eof` 只处理对应 channel 的状态，处理完后从 map 移除。
- 关闭 channel 时清理状态。
- `GitSshChannelState` 记下视图错误的退出码；`channel_eof` 按它发 exit-status，缺省为 0（HP-34）。

### SSH 视图错误的 ERR 与退出码（HP-34）

设计 [§6.1 的错误契约](history-projection.md#61-url-与路由决策)规定 SSH 对视图不存在和只读分别发送 `ERR view not found`、`ERR view URLs are read-only`，退出码均为 1；暂不可用发送 `ERR view <filter_id> unavailable: <reason>`，退出码为 75；打包预检失败发送错误原文的 `ERR`，退出码为 1。每条消息都是一个 pkt-line。

exec 阶段由 `reject_exec` 依次发送 `channel_success`、ERR、exit-status、EOF 和 close，不登记通道状态，并移除该通道的 `clients` 句柄与 v2 标记。russh 在 close 之后不再回调 `channel_eof` 或 `channel_close`。HP-18 与 HP-20 的 exec 拒绝经 `SshServer::send_exec_err` 调用这条路径。

v0 upload-pack、v2 `ls-refs` 和 v2 `fetch` 的数据阶段都经 `write_data_error` 写出视图错误。写出 ERR 后，通道不再写 data 字节，后续数据和 EOF 残留请求都会丢弃。`channel_eof` 先补处理残留请求，再取得退出码；若 `clients` 已没有该通道，说明它已被 `reject_exec` 拒绝，可能是 ERR 进入 russh 待发队列后客户端仍发送 EOF，此时只发送 close，exec 阶段发送的码是该通道唯一的 exit-status。非视图错误继续输出裸文本（`DEFER-HP-17`）。

### 视图 URL 入口第 1、2 层（HP-18）

定位符在 HTTP、SSH 与 LFS 入口共用同步分类，先识别首段视图保留名，再判定名字、版本与 filter ID 语法。HTTP 在分派 Git handler 前、SSH 在建立 `SmartSession` 前，以一次配置快照判定是否启用视图、服务是否只读。启用状态下的 upload-pack 在第 3 层接入前暂返回「不存在」。位置、顺序与状态码见[设计 §6.1「入口分层」和「错误契约」](history-projection.md#61-url-与路由决策)。

SSH `git-lfs-authenticate` 与 `git-lfs-transfer` 对视图路径写一行 `ERR view not found` 并以退出码 1 结束，与 HTTP LFS 的 404 同类；这是设计 §6.1 第 2 层「同样拒绝」的具体形式。

本卡 `integration_git_ssh` 中的原始 SSH 请求和 git 客户端沿用该 target 既有 storage-only 用例的宿主回环方式：宿主 `ssh` 和 git 经 `127.0.0.1` 连接。请求与应答字节不取决于执行位置；宿主 git 只断言 `not found`、`returned error: 403`、`remote error: view not found`、`remote error: view URLs are read-only` 四种稳定子串。此执行位置按[设计 §7.4 的 HP-18 例外](history-projection.md#74-测试计划)登记。

### 视图 URL 入口第 3 层（HP-20）

定位符通过认证后才解析；HTTP `git_info_refs` 在认证分支之后、v2 能力宣告短路之前解析，HTTP `git_upload_pack` 在 `check_upload_pack_access` 与读取请求体之间解析，SSH `exec_request` 在 `check_upload_pack_access` 之后、v2 短路之前解析。未认证请求不查视图表。具体顺序及应答见[设计 §6.1「入口分层」「认证先于解析」「逐请求解析」和「错误契约」](history-projection.md#61-url-与路由决策)。

HTTP 每个请求解析一次；SSH 每个 exec 解析一次，`ResolvedView` 随通道状态中的 `SmartSession` 保存。该 exec 内的各条协议命令各建一个 `ViewRepo`，共用 exec 时取得的配置快照。

### SSH upload-pack 可能把二进制 ACK/pack 数据当 UTF-8 发送

`handle_upload_pack` 当前：

```rust
session.data(channel, String::from_utf8(buf.to_vec()).unwrap()).unwrap();
```

`buf` 是 pkt-line bytes，虽然当前 ACK/NAK 文本大多是 UTF-8，但协议层应按 bytes 处理，不应经过 `String::from_utf8`。

建议：

- 所有 Git protocol payload 统一用 bytes 发送。
- 删除协议二进制路径上的 UTF-8 假设。
- `session.data` 调用只传 `Vec<u8>` 或 bytes，不做 String 转换。

### capability advertisement 与实际实现已对齐

历史风险：曾 advertise 一些未完整实现或解析不足的能力。当前状态（均已解决）：

- `include-tag`：当前发布版已从upload-pack advertise移除；MTR13候选通过`supports_include_tag`仅向ImportRepo通告，v0从首want能力、v2从fetch参数传递请求级选项，默认handler不通告、不附带。附带集合与负向限制唯一事实源见[traces契约§6](traces.md#6-普通代码-clone-与旧-git-traces)，须完整验收/C-D后生效。
- `delete-refs`：已重新 advertise（HTTP smoke 覆盖 CL delete；Monorepo 拒绝 Git-client tag；SSH delete 矩阵仍待补齐）。
- `atomic`：已从 advertise 移除（未实现原子 ref 更新）。
- `quiet`：已从 advertise 移除（未实现 progress 抑制语义）。
- `no-thin`：**（2026-09-16）重新 advertise**。移除期间任何 delta 压缩触底服务端已知对象的 push 都会以 thin pack 到达，而 pack 解码器对包外基对象 fail-closed（`git-internal` decode "Pack references bases that are not in the pack"），导致 receive-pack 连接中断、ref 不推进（缺陷 RCV-01：git 2.34/2.49 复现，`pack.window=0` 可绕过）。`no-thin` 语义（客户端不得发送 thin pack）正是解码器当前契约的正确配套，与上游 Mega 一致；同时 `unpack_stream` 将解码失败由 panic 收敛为 typed `ProtocolError`（2026-09-16）。
- `report-status-v2`：已从 advertise 移除（未实现 v2 完整语义）。
- `ofs-delta`：仍 advertise；OFS_DELTA pack 编解码由 `git-internal` crate 实现并自测（`internal/pack/decode.rs` 处理 offset delta），mega2 侧覆盖 advertise/parse（`parse_capabilities_recognizes_ofs_delta`）。

建议：

- 建立 capability truth table：advertise、parse、act-on、test 四列。
- 没有行为支持和测试的 capability 先不要 advertise。
- 历史已完成首批（MTR13 include-tag候选另见上条）：`atomic`、`report-status-v2`、`quiet`、`no-thin` 和 upload `include-tag` 已先从 advertise 移除；`delete-refs` 已在 HTTP CL delete smoke 覆盖后重新声明（Monorepo Tag 通过 API 管理）。
- ✅ 对 `object-format=sha1` 明确策略：SHA-1 为协议默认格式，SHA-1-only server 不 advertise `object-format`（协议允许 SHA-1 默认时省略；mega2 策略对 SHA-1-only repo 不 advertise），由单测 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` 锁定。

### pkt-line parser 错误模型已收敛，streaming reader 仍待后续

`src/ceres/protocol/smart.rs::try_read_pkt_line` 已定义 `PktLine` enum：
`Data(Bytes)`、`Flush`、`Delim`、`ResponseEnd`，并返回
`Result<PktLine, ProtocolError>`。Malformed input 不再通过 `unwrap` / panic
处理，而是显式返回 `ProtocolError::InvalidInput`，且失败时不消费输入
buffer。

已覆盖的边界：

- 空输入或不足 4 字节的 length header。
- 非 hex length header。
- reserved length `0003` 与 length 小于 header 的场景。
- length 大于剩余 buffer。
- `0000` flush-pkt、`0001` delim-pkt、`0002` response-end-pkt。

仍待后续：

- HTTP 和 SSH 当前仍先完整缓冲 request/channel 数据，再复用该 parser 解析。
  后续需要抽出真正的 streaming pkt-line reader，避免大请求完整驻留内存。

### upload-pack negotiation 已支持 shallow / protocol v2 / blob:none

早期实现只处理 `want`、`have`、`done`。当前已补齐现代客户端常用的部分扩展：

- protocol v1 upload-pack 解析 `deepen` / `deepen-relative`，并返回 `shallow` response。
- `deepen-since` / `deepen-not` 会明确返回 `ProtocolError::InvalidInput`，不 silent misbehave。
- HTTP 和 SSH 均支持 protocol v2 capability advertisement、`ls-refs` 和 `fetch`。
- protocol v2 `ls-refs` 在 head 为零 ID 时不写 HEAD 行，因此不会输出
  `capabilities^{}` 伪 ref 或 `symref-target`；
  `ls_refs_zero_head_flush_only` 锁定空 ref 列表只返回 flush。v0 `info/refs`
  保留伪 ref，以承载 capability 列表。
- protocol v2 `fetch` 支持 `filter blob:none`，pack 只发送 commits + trees，不发送 blob objects。

仍需后续确认或扩展：

- promisor remote
- tree filters（如 `tree:<depth>`）
- `want-ref`
- 大仓库 fetch 性能

建议：

- 用真实 Git CLI 矩阵验证 `git clone --depth=1`、HTTP/SSH v2 fetch、`git clone --filter=blob:none` 的端到端行为。
- 对未实现 filter spec（例如 tree filters）继续返回明确协议错误或 fallback，不 advertise 超出实现范围的语义。
- 中期将 upload-pack negotiation 改为 streaming pkt-line reader，减少大仓库 fetch 的 request body 聚合风险。

### receive-pack 原子性与并发语义需要收敛

当前 receive-pack 在 unpack 后更新 refs 和触发 side effects。代码里已有 monorepo/import 的 finalize 事务设计，但 advertised `atomic` 表示客户端可以期待“全部 ref 更新要么都成功，要么都失败”。

风险：

- tag 更新在循环中较早持久化，branch finalize 后续失败时是否能整体回滚需要确认。
- monorepo attach、CL、conversation、build hook 等 side effects 与 ref update 的事务边界需要明确。
- 并发 push 的锁粒度和 race 行为需要测试。

建议：

- 若不能保证 Git 意义上的 atomic，先不要 advertise `atomic`。
- 建立 receive-pack transaction boundary 文档。
- 对并发 push 同一 ref、非 fast-forward、删除 ref、tag update、mixed branch/tag push 建集成测试。

### HTTP push 认证与 fetch 认证策略不一致

当前 HTTP receive-pack 需要 Bearer 或 Basic Auth token。upload-pack 没有认证检查。SSH 侧通过 public key 认证后允许 upload-pack/receive-pack。

这可能是有意设计，但需要明确策略：

- public repo 是否允许匿名 clone/fetch。
- private repo 是否需要 fetch 认证。
- push 权限是否只验证“用户存在”，还是要验证 repo/path 权限。
- SSH authenticated user 是否写入 `SmartSession.auth` 用于 commit binding。

当前 HTTP receive-pack 会设置 `authenticated_user`，但 SSH publickey 认证后没有显式把 username 注入 `SmartSession.auth.authenticated_user`。这可能导致 SSH push 的 commit binding 变成 anonymous。

建议：

- 建立统一的 `ProtocolAuthContext`。
- HTTP 和 SSH 都在进入 `SmartSession` 前完成认证与授权。
- SSH auth_publickey 成功时保存 username，并传入 receive-pack session。
- 明确 fetch 是否需要认证；如果需要，upload-pack 也要走相同 authz。

### Git LFS SSH 仅支持 hybrid 模式

SSH 中 `git-lfs-transfer` 返回明确 unsupported failure，`git-lfs-authenticate` 返回 HTTP LFS URL。这符合 Git LFS 可 fallback 的 hybrid 模式，但不是纯 SSH LFS transfer。

建议：

- 文档明确：当前支持 Git LFS hybrid SSH -> HTTP，不支持 pure SSH LFS transfer。
- `git-lfs-transfer` 应返回明确的 unsupported/failure，而不是普通文本导致客户端误判。
- `git-lfs-authenticate` 返回 body 应符合 Git LFS SSH adapter 预期，包括 href、header、expires_at，以及 upload/download operation 差异。
- LFS HTTP endpoints 应补齐认证和 repo path 绑定，避免所有 repo 共用同一 `/info/lfs` 语义造成隔离问题。

### 路由匹配过宽，容易吞掉非 Git 路径

**2026-06-28 更新**：已抽出 `GitProtocolPath` parser（`src/contract/git_protocol/path.rs`），统一处理 repo path、仅去除末尾 `.git` suffix、service endpoint 与 HTTP 方法校验，并保留 `third-party.git` root repo 禁用规则。`src/server/http_server.rs::handle_smart_protocol` 不再内联后缀判断和 404 `unwrap()`，而是直接调用 parser。剩余工作：404/400/403/405 返回策略可随认证统一进一步标准化。

**2026-10-20（MTR-94）**：HTTP Router 外层对禁用 traces 的 GET/POST 保留命名空间请求先返回 HTTP 404（JSON 错误消息为 `traces locator not found`），覆盖 smart discovery/upload-pack/receive-pack，避免 v2 discovery、receive 探测和请求解压绕过关闭入口。根段规则同时检查原始 URI 与一次百分号解码后的字节路径；只移除 Git endpoint 前仓库名的一个末尾 `.git`，不把普通嵌套 `.traces` 或 `.traces-backup` 当成保留根。拒绝先于业务鉴权、body 消费和 Native/Import 分派，不查询 scope 存在性。 LFS 重写前的 URI 由服务端保存在请求扩展，禁用门同时检查它与当前路由 URI，避免移除仓库前缀后丢失保留根。字面根名为 `%2etraces` 的代码仓，其 `/%2etraces/…` URL 在 disabled 时同样拒绝；enabled 时保持既有分派。具体关闭语义及尚未接通的 enabled 数据面见 [traces 协议](./traces.md)。

**2026-10-21（MTR-100）**：SSH 在既有连接认证及 exec 解析后检查关闭开关；根 `.traces` 请求在 `SmartSession`、广告及 stdin 消费之前返回 `ERR traces locator not found`、exit status 1、EOF 并关闭通道。匹配字面路径及点段折叠，不作 HTTP 百分号解码；普通认证、普通代码分派与 storage-only receive-pack 限制不变。该守卫不启用 traces 数据面；匹配规则与实际验收边界见 [traces 协议](./traces.md) 和[执行计划](../plan/plan-20261015.md)。

**2026-10-23（MTR-97 候选）**：内部 wire 解码准入采用可取消异步输入与有界解码worker；固定限额、生命周期和当前验收状态见 [traces 协议](./traces.md) 与 [MTR-97 任务卡](../plan/plan-20261015.md)。真实 HTTP/SSH 数据面仍由后继卡接线。

历史建议（已落地）：

- 抽出 `GitProtocolPath` parser，统一处理 repo path、`.git` suffix、service endpoint。
- 路由层只做粗分发，路径语义在 parser 中测试。
- 404/400/403/405 返回策略标准化。

## 改进计划

### 阶段 0：兼容性基线测试

目标：在修改实现前建立可重复的真实 Git 客户端测试矩阵。

已新增 `scripts/git_protocol_smoke.sh` 作为第一版真实 Git CLI smoke matrix。该脚本面向已经启动的 mega2 HTTP/SSH 服务，默认覆盖只读 HTTP 场景（基础 clone/fetch、shallow clone、protocol v2 fetch、`filter blob:none`）；SSH 通过 `MEGA2_SSH_REPO_URL` 启用同类只读场景；HTTP/SSH **CL push**（断言不新增公开 `refs/heads/*`）与 **Git 客户端 Tag 推送拒绝** 通过 `MEGA2_GIT_SMOKE_PUSH=1` 显式启用；HTTP LFS push/clone/locks-list 通过 `MEGA2_GIT_SMOKE_LFS=1` + `MEGA2_GIT_SMOKE_PUSH=1` 显式启用，LFS 用例会从远端默认分支创建普通子提交、显式配置 `<remote>/info/lfs` endpoint，并对实际生成的 `refs/cl/*` 做定向 fetch、checkout、LFS pull 与清理，避免默认修改远端公开分支、不依赖孤儿初始化、全量 clone 或 Git LFS 对根路径 remote 的默认 URL discovery。CI 当前自动启用 HTTP read-only 矩阵、HTTP CL push + tag-reject 和 HTTP LFS round-trip；SSH push 仍保留为手动 opt-in。用户可见规则见 [`../user-guide.zh.md`](../user-guide.zh.md)。

**（2026-06-30）CI 自动化回归 gate 已落地；2026-07-01 扩展 CL push 与 LFS；2026-08-03 对齐 Monorepo 分支/tag 规则）**：`.github/workflows/git-protocol-smoke.yml` 在 PR 和 main push 时自动启动 PostgreSQL + Redis 测试栈、构建 release 二进制、seed mail secret、启动 `service http`，然后以 monorepo 根路径 `http://ci-smoke:<masked-token>@127.0.0.1:9000/` 为目标运行 `scripts/git_protocol_smoke.sh` 的 HTTP 矩阵（ls-remote、clone、fetch、protocol v2 fetch/ls-remote、shallow clone depth=1、blob:none partial clone、CL push（无新公开分支）、**拒绝** Git-client tag push、LFS push/clone/pull/locks-list）。workflow 直接在 smoke DB 的 `access_token` 表插入一次性 token 并 mask 日志输出，满足 push 所需认证；同时安装 `git-lfs` 并启用 `MEGA2_GIT_SMOKE_LFS=1`，让 LFS round-trip 成为默认 CI gate。该 workflow 在协议路径变更（`src/ceres/protocol/**`、`src/contract/git_protocol/**`、`src/server/http_server.rs`、`scripts/git_protocol_smoke.sh`、`docs/refactoring/protocol.md`）时触发，满足阶段 0 "每个后续阶段都能复用该矩阵防止回归"的验收标准。

### 视图打包预检（HP-23）

`prepare_pack` 以单条 SQL 对就绪状态、根链和 want 归属做单快照复查。未就绪或 want 已被回收时 reason 为 `warming up`；根链停追时为 `root chain halted`。计数阶段读取待发 tree 的存储字节，对 L0 `mega_tree` 行验证哈希，不自洽时拒绝并报告 `does not match its stored entries`；tree 缺失或无法解析时按缺对象处理，报告 `is missing`。这两种拒绝不改变视图就绪状态。

每次 L0 拒绝使 `view_pack_tree_mismatch_total` 加一，并发出含 `metric`、`filter_id`、`tree_id` 的 ERROR 事件。缺对象也发出含 `filter_id`、`tree_id` 的 ERROR 事件，不带 `metric`，不增加该计数。v2 的 `NAK` 轮不做打包预检。打包方法只按计数结果编码，不再读取 `mega_view_commit_map`。四类失败的线上应答见[历史投影设计 §6.1 错误契约](history-projection.md)。

### 场景覆盖表（权威，plan-20260803 / ADR-GM-01）

> **口径（强制）:** `git clone` / `git fetch` **不等于**覆盖字面 `git pull`。字面 pull 仅以下方 `cargo:…_pull_…` 格计；不得用 clone/fetch 冒充 pull 覆盖。

单元格取值只能是：`cargo:<exact_fn>`、`smoke:<case>`、`DEFER-GM-*`、或 `N/A+理由`。本表是唯一完整矩阵；`integration.md` 只保留 target 索引与回链。

同一格需要多个用例时，各取值以 `、` 分隔，每个取值仍须是上述形式之一。

| 用户命令 | HTTP | SSH | Auth | Repo-shape |
|---|---|---|---|---|
| ls-remote | smoke:HTTP_ls-remote | smoke:SSH_ls-remote | N/A+只读广告默认匿名 | N/A+Monorepo根路径；ImportRepo见DEFER-GM-01 |
| v2 ls-refs 空仓库（不存在的 `/<path>.git`） | cargo:integration_git_cli_v2_missing_path_empty | N/A+与 HTTP 共用 `handle_v2_ls_refs`，应答字节由 `ls_refs_missing_monorepo_path_flush_only` 锁定 | N/A+只读广告默认匿名 | N/A+Monorepo 不存在的路径；ImportRepo 缺失返回 NotFound |
| clone | cargo:integration_git_cli_http_round_trip | cargo:integration_git_ssh_authenticated_clone | cargo:integration_git_cli_auth_anonymous_disabled_rejects_clone | cargo:integration_git_cli_failpath_clone_missing_repo_keeps_service_alive |
| fetch | smoke:HTTP_fetch | smoke:SSH_fetch | N/A+随clone/token往返覆盖 | N/A+Monorepo；ImportRepo见DEFER-GM-01 |
| pull（字面） | cargo:integration_git_cli_http_pull_cl_ref_round_trip | cargo:integration_git_ssh_pull_cl_ref_round_trip | N/A+pull用例内鉴权与HTTP匿名默认 | N/A+定向refs/cl；非默认main快进 |
| push（CL） | cargo:integration_git_cli_http_round_trip | cargo:integration_git_ssh_authenticated_push_creates_cl_ref | cargo:integration_git_cli_auth_push_without_token_returns_401_challenge | N/A+Monorepo不新建refs/heads |
| CL ref delete（清理） | smoke:HTTP_push_CL | smoke:SSH_push_CL | N/A+删除随push_CL清理；无独立Auth用例 | N/A+仅删除本用例生成的refs/cl；不覆盖公开分支删除 |
| tag push（拒绝） | cargo:integration_git_cli_http_rejects_git_client_tag_push | smoke:SSH_reject_Git-client_tag_push | N/A+拒绝路径不依赖token形态 | N/A+Monorepo禁客户端tag创建 |
| tag delete（拒绝） | N/A+本计划未单列 tag 删除拒绝用例；Monorepo 禁止 Git 客户端 Tag | N/A+同 HTTP；SSH 未单列 tag 删除拒绝 | N/A+非鉴权矩阵轴 | N/A+公开 refs/tags 删除非本计划目标 |
| branch delete（公开） | N/A+Monorepo不提供公开refs/heads删除场景 | N/A+同HTTP | N/A+非鉴权矩阵轴 | N/A+公开分支删除非目标 |
| LFS push | cargo:integration_git_lfs_http_round_trip | DEFER-GM-02 | N/A+LFS往返内token | N/A+Monorepo+显式info/lfs |
| LFS pull | cargo:integration_git_lfs_http_round_trip | DEFER-GM-02 | N/A+随LFS往返 | N/A+定向CL取回后git_lfs_pull |
| LFS locks（list） | smoke:HTTP_LFS_push_and_clone | DEFER-GM-02 | N/A+随LFS smoke | N/A+只读list |
| SSH lifecycle/topology | N/A+HTTP不经sshd | cargo:integration_git_ssh_service_lifecycle_isolated | cargo:integration_git_ssh_wrong_key_is_rejected | N/A+cargo-native自启拓扑 |
| Mega refs fixture | cargo:integration_git_cli_mega_fixture_refs_round_trip | N/A+夹具守卫留在HTTP target | N/A+夹具不改鉴权面 | N/A+GM-09 go/no-go后启用或取消 |
| shell SSH 广度 CI | N/A+HTTP已有CI smoke | DEFER-GM-04 | N/A+DEFER范围 | N/A+DEFER范围 |
| missing-repo 客户端失败 | DEFER-GM-05 | DEFER-GM-05 | N/A+空仓广告语义 | N/A+原DEFER-IT-12→DEFER-GM-05由GM-11B收口 |
| 视图 receive-pack 拒绝 | cargo:integration_git_ssh_view_layer2_git_client_read_only | cargo:integration_git_ssh_view_layer2_ssh_read_only | cargo:layer2_receive_pack_forbidden_before_auth_and_body | N/A+视图前缀固定只读 |
| 视图 LFS 404 | cargo:layer2_lfs_not_found | cargo:integration_git_ssh_view_layer2_ssh_lfs_rejected | cargo:layer2_lfs_not_found | N/A+视图前缀不进入 LFS 命名空间 |
| 未知或已禁用的视图 | cargo:layer2_disabled_not_found、cargo:integration_git_cli_view_layer3_http_unknown | cargo:integration_git_ssh_view_layer2_ssh_not_found、cargo:integration_git_ssh_view_layer3_unknown | cargo:layer2_disabled_not_found、cargo:layer3_before_resolve_auth | cargo:layer2_enabled_unregistered_not_found |
| 视图 ls-remote | cargo:integration_git_cli_view_layer3_http_ls_remote | cargo:integration_git_ssh_view_layer3_ls_remote | cargo:layer3_before_resolve_auth | N/A+视图只有 ViewRepo 一种形态 |
| 视图 clone | cargo:integration_git_cli_view_pack_http_josh_proxy | cargo:integration_git_ssh_view_pack_ssh_josh_proxy | N/A+读授权与路径仓库相同 | cargo:integration_git_cli_view_pack_http_clone_gitlink |
| 视图 fetch | cargo:integration_git_cli_view_pack_http_fetch_after_trunk_push | cargo:integration_git_ssh_view_pack_ssh_josh_proxy | N/A+读授权与路径仓库相同 | N/A+视图只有 ViewRepo 一种形态 |
| 已就绪的空视图 | cargo:integration_git_cli_view_pack_http_clone_ready_empty | cargo:integration_git_ssh_view_pack_ssh_ready_empty | N/A+读授权与路径仓库相同 | N/A+视图只有 ViewRepo 一种形态 |
| 未就绪的视图 | cargo:integration_git_cli_view_layer3_http_unready | cargo:integration_git_ssh_view_layer3_unready | cargo:layer3_before_resolve_auth | N/A+视图只有 ViewRepo 一种形态 |
| 视图 fetch 非本视图提交（not our ref） | cargo:integration_git_cli_view_precheck_not_our_ref | cargo:integration_git_ssh_view_precheck_git_client_not_our_ref | N/A+只读，anonymous_access 默认放行 | N/A+视图 URL（/.filter/、/.view/） |
| 视图 clone 遇 L0 tree 不自洽 | cargo:integration_git_cli_view_precheck_l0_clone | cargo:integration_git_ssh_view_precheck_git_client_l0_clone | N/A+只读，anonymous_access 默认放行 | N/A+视图 URL（/.filter/、/.view/） |

#### GAP 映射（plan-20260803）

| GAP | 缺口 | 承接 |
|---|---|---|
| GAP-01 | 无字面 git pull | GM-02 |
| GAP-02 | anonymous_access=false 无真实 CLI | GM-03 |
| GAP-03 | LFS 不在 cargo 分层门 | GM-05 |
| GAP-04 | SSH 无确定性 cargo 拓扑与指纹播种 | GM-06 |
| GAP-05 | Mega 夹具许可/体积/生成方式未审计 | GM-09 |
| GAP-06 | 场景表与 target/CI 归属漂移 | GM-11B |
| GAP-07 | DEFER-IT-12 missing-repo 边界未收口 | DEFER-GM-05 |

历史建议清单（已被上表取代为权威单元格，仅保留作叙述背景）：

```text
HTTP:
  git ls-remote http://host/repo.git
  git clone http://host/repo.git
  git fetch
  git -c protocol.version=2 fetch
  git push HEAD:refs/heads/<tmp> → 期望新建 refs/cl/*，refs/heads/* 不变（`MEGA2_GIT_SMOKE_PUSH=1`）
  git push --delete origin refs/cl/<id>（清理；`MEGA2_GIT_SMOKE_PUSH=1`）
  git push origin refs/tags/<name> → 期望失败（Monorepo；`MEGA2_GIT_SMOKE_PUSH=1`）
  git clone --depth=1
  git -c protocol.version=2 ls-remote
  git -c protocol.version=2 clone --filter=blob:none

SSH:
  git ls-remote ssh://user@host:port/repo.git
  git clone ssh://user@host:port/repo.git
  git clone --depth=1 ssh://user@host:port/repo.git
  git -c protocol.version=2 ls-remote ssh://user@host:port/repo.git
  git -c protocol.version=2 clone --filter=blob:none ssh://user@host:port/repo.git
  git fetch
  git -c protocol.version=2 fetch
  git push HEAD:refs/heads/<tmp> → CL（同上；`MEGA2_GIT_SMOKE_PUSH=1`）
  git push origin refs/tags/<name> → 期望失败（`MEGA2_GIT_SMOKE_PUSH=1`）

LFS:
  git lfs install
  git lfs track
  git push with LFS object（`MEGA2_GIT_SMOKE_LFS=1` + `MEGA2_GIT_SMOKE_PUSH=1` 时启用）
  git clone with LFS object（`MEGA2_GIT_SMOKE_LFS=1` + `MEGA2_GIT_SMOKE_PUSH=1` 时启用）
  git lfs locks（`MEGA2_GIT_SMOKE_LFS=1` + `MEGA2_GIT_SMOKE_PUSH=1` 时启用，只读 list）
```

每个用例记录：

- Git 客户端版本。
- transport：HTTP 或 SSH。
- repo 类型：`ImportRepo` 或 `Monorepo`。
- auth 类型：anonymous、Bearer、Basic token、SSH key。
- 预期状态码或 SSH exit status。
- server log 中是否有 panic、敏感信息或 malformed pkt-line。

验收标准：

- 当前已支持能力有明确 passing/failing 表。
- 每个后续阶段都能复用该矩阵防止回归。

### 阶段 1：输入解析与错误模型止血

目标：消除协议入口上的 panic，让非法客户端输入返回可诊断错误。

工作项：

1. `info/refs` query 解析改为 `Result`。
2. 缺失、重复、额外参数或非法 `service` 返回 400。
3. SSH exec command 解析改为显式 parser。
4. 非法 SSH command 返回 channel failure，不默认 upload-pack。
5. `read_pkt_line` 改为返回 `Result`。
6. 所有协议二进制 payload 按 bytes 处理，不经过 UTF-8 String。

验收标准：

- malformed HTTP query 不 panic。
- malformed SSH exec 不 panic。
- malformed pkt-line 不 panic。
- Git 客户端收到明确失败，而不是连接被异常关闭。

### 阶段 2：统一 pkt-line / receive-pack streaming parser

目标：用协议边界替代 `PACK` magic 搜索。

工作项：

1. 已完成首批：receive-pack 先读取 command list 到 flush-pkt。
2. 已完成首批：flush-pkt 后剩余 bytes 作为 pack stream。
3. 后续：实现 streaming pkt-line reader，避免完整 body / channel 数据缓冲。
4. ✅ 支持无 pack 的 delete-only **和** pack-less create/update：flush-pkt 后剩余为空时跳过 `unpack_stream`/`receiver_handler`；非空剩余必须仍以 `PACK` 开头。pack-less 非删除命令要求目标 object 已存在（branch → commit，tag → 任意已存 object），否则 `ng … target object {id} not found`。
5. ✅ 空命令列表校验：splitter 在 flush-pkt 前无 command 时返回 `ProtocolError::InvalidInput`，避免无命令请求进入 ref 处理。
6. 已完成首批：HTTP 和 SSH receive-pack 共用同一 parser。
7. ✅ FC-08：monorepo 不允许的 tag 与 `refs/heads/main` 删除在持久化前 `ng`；失败 tag 不再二次 `update_refs`；仅至少一个 `ok` 的 branch 命令才 `finalize_receive_pack`（tag-only 不 finalize）；多余非删除 branch 按命令 `ng`（第一条仍可 finalize），而不是整包拒绝。CL 删除（delete-only）仍允许。
8. ✅ FC-09：receive-pack 删除按 advertised old id 条件删除；CAS 未命中时不删除已被并发更新的 ref，报告 `moved since advertisement`。

验收标准：

- 已覆盖：command payload / capability 中出现 `PACK` 不误切分。
- 待覆盖：`PACK` 跨 chunk 不影响 push（需要 streaming parser 或真实 Git CLI 矩阵）。
- ✅ `git push --delete` 可正常返回 report-status（delete-only 跳过 unpack，由 `is_delete_only_push_detects_pure_delete_vs_mixed` 锁定）。
- ✅ pack-less 非删除请求被 splitter 接受（由 `split_receive_pack_request_accepts_pack_less_non_delete` 锁定）。
- ✅ 空命令列表返回协议错误（由 `split_receive_pack_request_rejects_empty_command_list` 锁定）。
- malformed command list 返回协议错误。

### 阶段 3：capability truth table 与 advertise 收敛

目标：只 advertise 真正支持且有测试的 capability。

工作项：

1. 建立 capability truth table。
2. 移除或补齐 `atomic`、`report-status-v2`、`quiet`、`delete-refs` 等能力；include-tag按上文MTR13候选区分handler，待自身C/D。
3. 明确 `ofs-delta`、`no-thin`、`side-band-64k` 的 encode/decode 测试。（✅ 2026-09-16：`no-thin` 由 advertise 断言 + RCV-01 端到端回归覆盖，见上表与 truth table）
4. ✅ 对 SHA-1 object format 做显式策略：SHA-1 为协议默认格式，SHA-1-only server 不 advertise `object-format`（协议允许 SHA-1 默认时省略；mega2 策略对 SHA-1-only repo 不 advertise），由 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` 锁定。

验收标准：

- advertise 的每个 capability 都有 parse/act-on/test 证据。
- 标准 Git 客户端不会因为误导性 capability 进入未实现路径。

#### Capability Truth Table（2026-06-23 建立）

| Capability | Advertised? | Parsed? | Acted On? | Tested? | 说明 |
|---|---|---|---|---|---|
| `report-status` | ✅ receive-pack | ✅ | ✅ 生成 `unpack ok` + per-ref status | ✅ `receive_pack_advertises_only_supported_baseline_capabilities` | 基础 report-status v1 语义 |
| `side-band-64k` | ✅ both | ✅ | ✅ `build_side_band_format` | ✅ `build_side_band_format_wraps_payload_when_side_band_64k_enabled` + `build_side_band_format_passthrough_when_capability_absent` | pack data 通过 side-band 传输 |
| `ofs-delta` | ✅ both | ✅ | ✅ pack decode 委托 `git-internal`（支持 offset delta 编解码，见 `git-internal::internal::pack::decode`） | ✅ advertise/parse：`parse_capabilities_recognizes_ofs_delta` + advertise 断言（pack decode 由 `git-internal` 自测） | OFS_DELTA pack 编解码由 `git-internal` 实现并自测；mega2 侧仅覆盖 advertise/parse |
| `multi_ack_detailed` | ✅ upload-pack | ✅ | ✅ negotiation ACK 逻辑 | ✅ `parse_capabilities` 单测 | upload-pack negotiation |
| `no-done` | ✅ upload-pack | ✅ | ✅ 与 multi_ack_detailed 联动 | ✅ negotiation 单测 | 允许在 multi_ack_detailed 下提前发 pack |
| `shallow` | ✅ upload-pack | ✅ | ✅ `deepen` / `deepen-relative` 生成 shallow pack 和 `shallow` response；ViewRepo（P0）不覆写 `supports_shallow_fetch`，HTTP v0/v2 depth 返回 400；SSH 同样拒绝但写裸文本 `error: …`（DEFER-HP-17） | ✅ capability parse/advertise 单测 + shallow traversal 单测 | protocol v1 shallow clone 基础语义 |
| `ls-refs` | ✅ protocol v2 | ✅ | ✅ 处理 `ref-prefix`、`symrefs`、`peel` | ✅ v2 capability / command parse 单测 | protocol v2 refs discovery |
| `fetch=shallow filter` | ✅ protocol v2 | ✅ | ✅ v2 fetch 支持 `want`/`have`/`done`、`deepen`、`filter blob:none`；ViewRepo（P0）不覆写 `supports_filtered_fetch`，HTTP v2 depth/filter 返回 400，SSH 同样拒绝但写裸文本 `error: …`（DEFER-HP-17）；v0 不宣告 filter | ✅ v2 command parse 单测 + pack generation gates + capability honesty 单测 | v2 fetch；`filter blob:none` 只发送 commit/tree objects；`RepoHandler::supports_shallow_fetch`/`supports_filtered_fetch` 门控 |
| `agent=mega/0.1.0` | ✅ both | ❌ | ❌ | ❌ | 信息性，不影响协议行为 |
| `atomic` | ❌ 已移除 | ✅ | ❌ | N/A | 未实现原子 ref 更新，已从 advertise 移除 |
| `report-status-v2` | ❌ 已移除 | ✅ | ❌ | N/A | 未实现 v2 语义，已从 advertise 移除 |
| `delete-refs` | ✅ 已声明 | ✅ HTTP CL delete + Monorepo tag-reject smoke | ❌ | N/A | HTTP smoke 已覆盖 CL delete；Monorepo 禁止 Git-client Tag；SSH delete 矩阵仍未补齐 |
| `quiet` | ❌ 已移除 | ❌ | ❌ | N/A | 未实现 progress 抑制，已从 advertise 移除 |
| `no-thin` | ✅ receive-pack（2026-09-16 重新 advertise） | ❌（客户端侧语义，服务端无需 parse） | ✅ 依赖客户端不发送 thin pack；服务端解码器对残余 thin/malformed pack fail-closed 并经 `unpack_stream` 转为 typed 错误（2026-09-16） | ✅ `receive_pack_advertises_only_supported_baseline_capabilities` + RCV-01 端到端回归（默认 delta push） | 移除期间 thin pack 触发解码 panic/断连（RCV-01）；重 advertise 以匹配解码器 fail-closed 契约 |
| `include-tag` | 发布版移除；MTR13候选仅ImportRepo通告 | 候选v0首want/v2 fetch解析 | 候选按请求附带T(B) | MTR13精确集合/负控待验 | Monorepo/ViewRepo保持不通告、不附带；候选未通过自身C/D |
| `server-option` | ❌ 已移除 (v2) | ✅ | ❌ | ✅ `v2_capability_advertisement_does_not_advertise_server_option` | v2 parsed capabilities 未被 inspect/act-on，advertise 会误导客户端；2026-06-30 从 v2 advertise 移除 |
| `object-format` | ❌ protocol v1；✅ protocol v2 `object-format=sha1` | ✅ v2 capability advertisement | N/A | ✅ v1 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` + v2 capability 单测 | v1 保持 SHA-1 默认不 advertise；v2 capability list 显式声明 `sha1` |

残余风险：`ofs-delta` 的 pack 编解码由 `git-internal` crate 实现并自测，mega2 侧已覆盖 advertise/parse；如需端到端 OFS_DELTA pack 矩阵可在 `git-internal` 侧补足。

### 阶段 4：认证与授权统一

目标：HTTP 和 SSH 使用同一协议认证语义。

工作项：

1. ✅ 定义协议认证上下文：`SmartSession.auth: AuthContext`（`username` + `authenticated_user: PushUserInfo`），HTTP 与 SSH 共用。
2. ✅ HTTP Bearer / Basic token 认证填充同一 context（`git_receive_pack_auth` → `SmartSession::set_authenticated_user`）。
3. ✅ SSH publickey 认证成功后保存 username（`SshServer.authenticated_user`），exec 阶段传入 `SmartSession`（`set_authenticated_user`），commit binding 不再匿名。
4. ✅ 明确 upload-pack 是否允许匿名访问：`check_upload_pack_access(git_config, auth)` 按 `git.anonymous_access`（默认 `true`，向后兼容）放行匿名 clone/fetch，关闭时无 token 返回 `ProtocolError::Forbidden`；HTTP `info/refs` upload 分支（`http.rs:53`）、`git_upload_pack`（`http.rs:217`）与 SSH（`ssh.rs:149`）共用，`mod.rs` 3 个单测锁定 allow/deny/authenticated。
5. 🔶 receive-pack 检查 repo/path 级 push 权限：`check_push_permission(state, auth, repo_path)` 未认证（无 `username`）直接 `Forbidden`，已认证时对 `Repository(repo_path)` 走 Cedar `pushRepo` 授权（`state.entity_store` 非空时按策略裁决，为空则放行）；HTTP receive-pack 两处（`http.rs:64/355`）与 SSH（`ssh.rs:140`）均在 unpack 前调用。更细粒度的 repo/path ACL 与基于策略的 push-deny 协议层单测待后续。
6. ✅ commit binding 使用同一 authenticated actor（`bind_commit_to_user` 读取 `auth.authenticated_user`，HTTP/SSH 路径统一）。

验收标准：

- ✅ HTTP push 和 SSH push 都能绑定到正确用户。
- 🔶 private repo fetch 策略明确且有测试：`check_upload_pack_access` 的匿名/关闭策略已有 3 个单测；`anonymous_access=false` 的端到端矩阵待补。
- 🔶 未授权（未认证）push 返回 `Forbidden`，不进入 unpack（`check_push_permission` 在 unpack 前拒绝无 token 请求）；基于策略的 push-deny 协议层单测待后续。

### 阶段 5：SSH per-channel state 与 LFS hybrid 加固

目标：提高 SSH 多 channel 和 LFS 客户端兼容性。

工作项：

1. 引入 `GitSshChannelState`，按 `ChannelId` 保存状态。
2. `channel_eof` 只处理当前 channel。
3. ✅ `git-lfs-transfer` 通过 SSH stderr extended-data 返回规范 unsupported 错误，并通过 channel failure 触发客户端 fallback。
4. ✅ `git-lfs-authenticate` / `git-lfs-transfer` 已校验 upload/download operation；`git-lfs-authenticate` 仍返回同一 HTTP LFS endpoint，后续如需按 operation 拆分 header/URL 再补。
5. ✅ LFS HTTP endpoint 已绑定认证上下文与 repo path。HTTP LFS 端点原先完全匿名（未套 `cedar_guard`）。**认证模型以 batch 端点为授权闸门**（`src/api/router/lfs_router.rs`）：`objects/batch` 按 `operation` 分流——`download` 为读、`upload` 为写。**形态分流（plan-20260909 / ADR-LF-01）**：`push_policy=review`（省略 `push_auth`）下，读遵循 `git.anonymous_access`（默认 `true`，与 upload-pack 一致），写要求有效 mono access token（Bearer 或 Basic 密码位，经 `UserStorage`）；`push_policy=trunk` 下写/读对齐 `git.push_auth`——`none` 时读写均可匿名（与匿名 receive-pack 同级网络前提）；`token` 时写要求 `[[git.push_tokens]]` 命中且 `paths` 覆盖 `LfsRepoContext`（空前缀视为 `/`），读仍可 `anonymous_access || token`。未授权写返回带 `WWW-Authenticate` 的 `401`，路径不覆盖返回 `403`。lock `create`/`unlock` 为写、`list`/`verify` 为读，同样按此策略。纯策略函数 `lfs_access_allowed`（review）、`enforce_trunk_lfs_access`（trunk）与 `enforce_lfs_access` 收敛决策；`token_covers_repo` / `lookup_push_token` 与 receive-pack 共用。**object 传输端点（`PUT`/`GET /objects/{oid}`）是 batch 下发的能力 URL，不再逐请求鉴权**：git-lfs 不会稳定地对 transfer 请求附带凭据（会导致上传 `PUT` 在 body 传输中途收到 `401` 而 broken pipe）。上传安全性由两层保证：(1) `lfs_upload_object`（handler）要求对象必须已由 `upload` batch 注册 metadata（否则 `Not found`），而 `upload` batch 走写鉴权闸门；(2) **内容寻址不可变**——`lfs_upload_object` 校验上传字节的 `sha256` 等于所声明的 OID 且大小等于注册 size，并对已存在对象跳过写入，因此即便匿名直连 `PUT` 也只能（重复）存入恰好哈希到该 OID 的字节，无法篡改或伪造对象。raw `GET` 为匿名（内容寻址能力 URL，绕过 `anonymous_access`）；OID 为 sha256 内容哈希，知道它即隐含已有该内容的引用。**repo path 绑定**：`rewrite_lfs_request_uri` 在剥离 `/info/lfs` 前缀前先把 repo 前缀存入 `LfsRepoContext` 请求扩展，lock 端点据此把锁行 key 命名空间化为 `{repo}\u{1f}{ref}`（`scoped_lock_ref`），使不同 repo 下同名 ref 的锁不再共用一行；object 由 OID 内容寻址，天然跨 repo 安全，无需隔离。空 repo（`/api/v1/lfs` 内部挂载）回退到裸 ref key 保持向后兼容。

验收标准：

- ✅ 单 SSH connection 多 channel 不串状态（`SshServer` 已按 `ChannelId` 隔离 `GitSshChannelState`）。
- ✅ Git LFS 客户端能稳定完成 HTTP LFS push/clone（object 传输不逐请求鉴权，避免 transfer `PUT` 因中途 `401` broken pipe；由 CI `git-protocol-smoke` 的 LFS round-trip 覆盖）。
- ✅ 未认证的 LFS 写操作（`objects/batch operation=upload`、lock create-unlock）返回 401，不再匿名可写；读操作遵循 `anonymous_access` 策略（由 `lfs_access_policy_matrix` 锁定）。匿名直连 object `PUT` 因缺少 batch 注册的 metadata 返回 `Not found`；即便针对已注册 OID，也因 `sha256`/size 校验与「已存在则跳过」而无法篡改或伪造。
- ✅ LFS lock 操作不会跨 repo 混淆：锁行 key 按 repo 命名空间化（由 `locks_are_namespaced_by_repository` 锁定）；object 内容寻址（OID）天然跨 repo 安全。

### 阶段 6：upload-pack 兼容性扩展

目标：提高 clone/fetch 在大仓库和现代 Git 客户端下的兼容性。

工作项：

1. ✅ 实现 `deepen` / shallow clone 基础语义：upload-pack 解析 `deepen` / `deepen-relative`，`Monorepo::shallow_pack` 做 depth-limited traversal，并返回 `shallow` pkt-lines。
2. ✅ 支持 `deepen-since`、`deepen-not` 或明确拒绝：当前明确返回 `ProtocolError::InvalidInput`。
3. ✅ 评估并实现 protocol v2 的 `ls-refs` 和 `fetch`：HTTP 通过 `Git-Protocol` header，SSH 通过 `GIT_PROTOCOL` env request。
4. ✅ 评估 partial clone filter：已实现 protocol v2 `filter blob:none`；tree filters 暂未实现完整语义，后续需按真实 Git CLI 矩阵决定是否扩展。

验收标准：

- `git clone --depth=1` 行为正确。
- 不支持的 modern feature 有明确错误或 fallback，不 silent misbehave。
- 大仓库 fetch 不需要一次性读取大请求体（仍待 streaming pkt-line reader 完成）。

## 推荐优先级

| 优先级 | 工作 | 原因 |
| --- | --- | --- |
| P0 | 建立真实 Git 客户端兼容性矩阵 | ✅ **已落地并扩展（2026-07-01）**：`scripts/git_protocol_smoke.sh` + `.github/workflows/git-protocol-smoke.yml` CI 自动化回归 gate，覆盖 HTTP read-only 场景（ls-remote/clone/fetch/v2 fetch/shallow/blob:none）、HTTP branch/tag push+delete 以及 HTTP LFS push/clone/pull/locks-list；SSH push/delete 保留为脚本手动 opt-in。**最小往返集（clone→push→再 clone + 用例隔离）已进 cargo target** `bin/tests/integration_git_cli.rs`（IT-03，**Linux 目标 OS**，`cargo test -p mega2 --test integration_git_cli`）；正路径广度矩阵仍以脚本为准，不在 `bin/tests/` 重复 |
| P0 | 修复 HTTP query、SSH exec、pkt-line parser 的 panic | 非法客户端输入不能打崩服务 |
| P0 | receive-pack 用 pkt-line flush 分界替代搜索 `PACK` | 已完成首批；HTTP branch/tag push+delete 已进入 CI；SSH push/delete 仍为手动 opt-in；后续补 streaming parser |
| P1 | capability truth table，移除未实现 advertise | 避免误导 Git 客户端进入未实现语义 |
| P1 | SSH payload 全部按 bytes 发送 | Git 协议是二进制协议，不能假设 UTF-8 |
| P1 | 统一 HTTP/SSH auth context | ✅ 已落地：HTTP/SSH 共用 `check_upload_pack_access`（匿名策略，可配置）+ `check_push_permission`（Cedar `pushRepo` 授权）；commit binding 绑定 authenticated actor。更细 repo/path ACL 与 push-deny 协议层单测待后续 |
| P2 | SSH per-channel state | ✅ 已实现（2026-06-28）：`SshServer` 按 `ChannelId` 维护独立 `GitSshChannelState` |
| P2 | LFS hybrid response 加固 | 提升 Git LFS 客户端兼容性 |
| P3 | tree filters / promisor remote / streaming upload-pack parser | 进一步优化现代 Git 客户端和大仓库体验；shallow clone、protocol v2、`filter blob:none` 已完成基础支持 |

## 建议的文档化兼容声明

在 README 或部署文档中，当前阶段建议明确声明：

- 支持 Git smart HTTP 的基础 clone/fetch/push，以及 protocol v2 `ls-refs` / `fetch`。
- 支持 Git SSH 的基础 clone/fetch/push，以及通过 `GIT_PROTOCOL=version=2` 启用 protocol v2 `ls-refs` / `fetch`。
- 支持 Git LFS HTTP endpoints，以及 SSH `git-lfs-authenticate` hybrid 模式。
- 暂不支持 pure SSH LFS transfer。
- 支持 shallow fetch/clone 基础语义（`deepen` / `deepen-relative`）；兼容性仍需要真实 Git CLI 矩阵确认。
- 支持 protocol v2 partial clone `filter blob:none`；暂不承诺 tree filters 或完整 promisor remote 语义。
- advertise 的 capability 以实现和测试为准，未实现能力不应对外声明。
- `object-format=blake3`（及 servable `sha256`）是 git-internal / Libra **extension**，不宣称与标准 Git 客户端互通。

## 下一步建议

第一批 PR 建议控制在 P0：

1. 新增真实 Git CLI smoke test 脚本或集成测试说明。
2. `info/refs` query 解析返回 `Result`，非法输入返回 400。
3. SSH exec command parser 独立成函数并加单元测试。
4. `read_pkt_line` 返回 `Result` 并覆盖 malformed input。
5. 已完成首批：receive-pack 按 pkt-line flush 分界，不再搜索 `PACK`。
6. 已完成首批：SSH upload-pack 初始响应删除 UTF-8 转换，payload 按 bytes 写回 channel。

完成这些之后，再开始 capability 收敛、认证统一和 LFS 加固。

## push 门三态（UN-02）

`check_push_permission` 切换到三态 helper（ADR-UN-01）：`off` 短路放行（默认，零影响）；`shadow` 放行但记录 would-deny（`event=authz_would_deny` 结构化字段）；`enforce` 拒绝无权限 push。资源经 UN-11 归一为根仓库 `Repository::"/"`，根实体缺失 fail-closed。首建 `ensure` 在 HTTP listener 绑定前完成（`start_http`）。

SSH 面（UN-03）与 HTTP 面共享同一 `AppContext.entity_store` 实例（`service multi` 单进程内四方恒等：context↔storage↔HTTP↔SSH）。独立 `service ssh` 在命令层对 `enforcement != off` 拒绝启动（指引改用 `service multi` 或保持 `off`）；SSH listener 绑定前完成幂等首建。

## 视图协议错误契约与预检钩子（HP-17）

`RepoHandler::authorize_fetch(want, have)` 的默认实现调用原
`check_wants_and_ready(want)` 并原样返回 haves；Monorepo/ViewRepo 保持原预检语义。
MTR-40 的 ImportRepo 覆盖它：在本次构建的代码视图中拒绝不可读 want（该视图由多次数据库读取形成，不是事务快照），将 traces、
已删除代码 ref 和未知本地 commit 的 have 从 ACK 与 pack 基础集移除；这些 have
不会使普通 fetch 失败。受污染代码 ref 从普通 upload 广告中隔离，显式请求其提交
返回稳定 `trace_history_in_code`。该钩子在 v0/v2 任意 ACK/NAK 前运行；原
`prepare_pack` 仍只在实际发包前运行，不改变 ViewRepo 非 ready 协商轮成本。
receive-pack 广告只向已通过当前推送权限检查的调用者显示受污染代码 ref，
供显式删除或重置；旧 traces ref 在两个方向均隐藏，标准 Git 客户端无法从广告取得它的更新旧值。该广告规则没有增加对已知旧 OID 的原始 receive 命令的服务端写入拒绝策略。
完整 PACK 对象闭包隔离仍由 [唯一 traces 协议](traces.md#6-普通代码-clone-与旧-git-traces)
和后继 MTR-13 承接：当前 full_pack 实际仍发送同仓全部对象，MTR-40 的 want/have
守卫不提供 traces 内容保密性。 非旧 traces 的 ImportRepo 若默认分支不是 main，v2 HEAD symref 也指向实际可见的默认分支；MTR-40 用例覆盖 dev 分支。

钩子失败时不会调用任何 pack 方法。`ViewUnavailable` 的 HTTP 应答是带 `Retry-After` 的 503；
`PackRejected` 的 HTTP 应答是 200、`application/x-git-upload-pack-result` 和一个 ERR
pkt-line。错误字段与完整应答表见 [错误文档](../errors.md#megaerrorviewunavailable--viewpackrejected视图协议错误plan-20261002)
和 [history-projection 设计 §6.1「错误契约」](history-projection.md#61-url-与路由决策)。视图 URL 的错误产生方和
黑盒覆盖由后续入口与打包卡登记，本节不重复场景覆盖表行。

**2026-10-24（MTR-98 自身Claude审查、v0.42.69完整C/D已核）**：内部发布事实由受控 PACK 与本 stream 已发布 raw 闭包推导，原始字节、取消、schema 归档及可见性边界见 [Traces 唯一协议源](./traces.md)。封闭事实桥尚未接入 HTTP/SSH；逐提交 checkpoint 历史资格仍待 MTR-101，不能据此放行 wire 发布。

**2026-10-25（MTR-101内部实现）**：逐提交历史校验及受控发布入口已完成真实A4 42例，P2覆盖与正文缺口已补齐；最终候选自身Claude ER05 R2及五门C已PASS，0.42.71已提交推送并发布，main/tag实际D全绿。原A3与ER05 R1按旧输入保留。显式基线、私有witness、错误分类与成本见[Traces唯一协议源](./traces.md#3-原始对象存储接收与恢复)。HTTP/SSH公开接线仍由后继任务交付。

> 原候选历史原文：**2026-10-25（MTR-101 未验收候选）**：逐提交历史校验与受控发布入口已在工作区接线，实际边界与待验收状态见 [Traces 唯一协议源](./traces.md)。当前尚无本卡 A、代码审查或 C/D 通过结论；HTTP/SSH 接线仍由后继任务交付。

<a id="ssh-dependency-security"></a>

### SSH 依赖安全基线（FIX-MTR-11）

`russh` 的实际锁定版本为 0.64.1，包含 [GHSA-j7h2-66j4-ghjf](https://github.com/Eugeny/russh/security/advisories/GHSA-j7h2-66j4-ghjf) 的公钥请求/签名算法一致性修复，以及 [GHSA-4wc5-f2rc-q74m](https://github.com/Eugeny/russh/security/advisories/GHSA-4wc5-f2rc-q74m) 的 agent 重复 lifetime 约束修复。后者先验证完整约束，再插入身份和创建过期任务；本项目未启用 agent 服务或转发入口。

0.64.0 移除的可选 `legacy-ed25519-pkcs8-parser` 在旧实际 feature 树中未启用；本次不新增 feature，不改变现有标准 OpenSSH 密钥、用户公钥指纹或 github_sync 密钥持久化接口。测试使用临时密钥和私有传输，负例只有两个短 lifetime 约束，不运行高放大资源耗尽演示。源码/验收/审查/发布与 GitHub 告警终态分开登记于[日期计划](../plan/plan-20261015.md)的 FIX-MTR-11；依赖升级本身不等于远端告警已经关闭。

### MTR99内部raw fetch验收边界（2026-10-25）

**当前指针：** MTR99内部raw流已由现行卡结清并被MTR11消费，HTTP scoped Git在v0.42.76已交付，scoped SSH仍待MTR39/92/12。当前限制与未结清的 `DEBT-MTR98-ROLLBACK-BOUND` 见[traces唯一契约当前义务](traces.md#8-联合验收与发布证据)及[MTR99任务卡](../plan/plan-20261015.md#task-mtr-99-scoped-published-raw对象流服务)；旧A1/R1和未接线状态只属下方历史，不补认回滚耗时或旧C2根因。

**历史验收边界（2026-10-25原文）：** 授权快照、闭包减集、取消和外存语义仅以[traces唯一契约](traces.md#mtr99当前内部raw-fetch实现与验收边界2026-10-25)及[MTR99任务卡](../plan/plan-20261015.md#task-mtr-99-scoped-published-raw对象流服务)为准。A1已通过，自己的R1源码/内存PASS但H0 FAIL正在前滚修订；未发布、HTTP/SSH未接线。磁盘TMPDIR、同步IO及完整闭包成本、stored parser错误分类由任务卡具名P2承接，不在此复制协议规则。

## MTR103发布后事实同步与原现行记录（2026-10-26）

本次仅同步已实际核验的MTR103收口，承载于MTR11既有治理I；尚未准入MTR11实现或认领A/C。下面保留此前被替换的现行列，属于C1前历史快照，不作为当前待办或窗口授权。

```text
> **现行收口与执行前沿（2026-10-26）：** 当前状态（2026-10-26，实际收口2026-10-25 22:39:40 UTC）：MTR-102已done/complete；最终C3完整21 targets、3116 PASS、0 failed、3原ignored，nightly fmt、all-target/all-feature严格Clippy及双build全部exit0/0warning；本地Claude自身/执行/发布审查PASS，root在原3600秒内核最终C。签名DCO提交fc48e4efb6a238ce52b5fdb68a1c7688f60fdd00已推main，v0.42.74原生签名标注tag与手写Release已核，main CI40239601722、tag CI40239634117含amd64/arm64/Docker manifest全部实际success，未用Harbor例外。正式Mega22/118、Libra原计划0/26、联合22/144；外部资格1/4。 当前依赖与后续门（2026-10-26）：102的完整C/D已解除103前驱；当前执行前沿MTR-103，in-progress/locally-accepted；候选四生产文件/24新测试已写入，最终源码与A1执行审Claude c2a70c27五PASS，A1七门实际exit0，10/13/3/11/105逐身份通过；root于2026-10-26 00:04:43 UTC在原1800秒内观察验收，596.534秒，无新增资源残留、无需人工清理。自己的ER05已由同一本地Claude1542e9ab补齐四PASS并经root实核148输入/模式采纳；完整C1及发布D仍待，候选0.42.75尚未bump，无本卡提交/push/版本发布。顺序103→11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2，各卡自身门与跨仓资格/WRITERS/LIVE前置不省略。Libra FIX04 C1仍FAIL/STOP，F1/F2 P1未修；实际Claude2891a30b来源PASS/发布次序FAIL。仅FIX-LTR-05一次release保留现有ingress两方法同一dead_code诊断的具名例外待用户答复，未准入新Libra实现/A/C/发布；PR636已满足旧Rustdoc下一提交要求。旧FIX3诊断3/3、B8失败及8/8 STOP、所有原失败和历史数据库根因未知均保留。
> 本次真实post-C/D记录由MTR103既有六Mega治理I承载，其最终C/D尚待；原Libra三治理仍仅本地、由LTR01承载。下方更早日期的当前/候选/待审措辞保留为历史，以本段、任务卡现行字段和H0现行列为准；不回改旧FAIL/STOP或消费过的预算。
```

## 历史快照：MTR11开工候选与Libra main交接同步（2026-10-26）

**历史快照（MTR11开工候选）：** 当前状态（2026-10-26，MTR11开工候选）：MTR103完整C/D已闭合，发布main 5a2b93d9ca5a4071dbb5d1a1a6c7a403742a6102/v0.42.75；当前源码未改、仅治理同步。正式Mega23/118、Libra原计划0/26、联合23/144；外部资格实现卡1/5（DOC01/02另计）。Libra单文件交接a4d84829d16205ce26317c6c9b8cdfc55e2ac4aa已单独提交并推main，原计划不因此完成。

**历史快照（MTR11开工候选）：** 当前依赖与后续门（2026-10-26）：MTR11前驱已闭合，本次源准入候选仍pending/空；当前ER01/02/03及本地Claude真实审查待，A/C未claim。依赖顺序11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2不变。Libra资格由用户安排另一机器Agent承接：FIX01→FIX05→FIX04→FIX02→FIX03；F2先做只读ATTRIB（可unknown），FIX05完整C/D后的同因CLOSE或独立FIX闭合必须在FIX04前；无循环依赖。仅FIX05一次release保留现有ingress两方法dead_code的具名例外仍未授权，不能由main交接推导。FIX04旧C1/F1/F2 FAIL与STOP、FIX02旧C1 STOP、所有旧预算及历史C2根因未知均保留。H0、资格、WRITERS、LIVE与H1仍分别核验。

Libra交接实际证据（2026-10-26）：单文件main提交a4d84829，parent b73e98a7、版本0.30.87不变；原生签名/DCO及远端blob88199291已核，plan SHA256 d969e2ddd50e67b3498018e2766c27fce39896c0dcd3f10d401de2755f1afaee。实际本地Claude R3/自身ER05均PASS，nightlyfmt/strictClippy与architecture guard65/65通过；03:32:21 UTC观察base40262985954与CodeQL40262985956 attempt1仍有运行/排队，未宣称D通过。只交付任务定义，不授FIX05警告例外、不bump/tag/Release；DOC02覆盖仍待FIX03。

本次只刷新当前事实与源码锚点；MTR11八AC、六VER、七生产文件/四落点、独立patch及一次A/C各3600秒保持。实际准入审查尚待，未开始源实现或验收。以下为本次替换前的历史现行内容，不能作新窗口授权。

```text
> **现行收口与执行前沿（2026-10-26）：** 当前状态（2026-10-26，实际收口01:54:13 UTC）：MTR-103已done/complete；C1完整21 targets、3140 PASS、0 failed、3原ignored，nightly fmt、all-target/all-feature严格Clippy及双build全部exit0/0warning；自己的本地Claude ER05与执行/发布审查均PASS，root在原3600秒内2298.567秒观察最终C。签名DCO提交5a2b93d9ca5a4071dbb5d1a1a6c7a403742a6102已推main，v0.42.75原生签名标注tag与手写Release已核；main CI40254455937、tag CI40254547920的amd64/arm64/Docker manifest全部attempt1实际success，未用Harbor例外。正式Mega23/118、Libra原计划0/26、联合23/144；外部资格1/4。 当前依赖与后续门（2026-10-26）：MTR102与103完整C/D已闭合；当前执行前沿MTR-11，pending/空，仅来源与开工核验，尚未准入实现或认领A/C。顺序11→40→13→39→92→12/H1→LTR10→11→12→LTR01/H2；各卡自身门与跨仓资格/WRITERS/LIVE前置不省略。Libra FIX04 C1仍FAIL/STOP，F1/F2 P1未修；实际Claude2891a30b来源PASS/发布次序FAIL。仅FIX-LTR-05一次release保留现有ingress两方法同一dead_code诊断的具名例外仍待用户明确答复；未准入新Libra实现/A/C/发布，PR636已满足旧Rustdoc下一提交要求。旧FIX3诊断3/3、B8失败及8/8 STOP、所有原失败和历史数据库根因未知均保留。
> 本次MTR103真实post-C/D事实由MTR11既有六Mega治理/协议I承载，其自身C/D尚待；原Libra三治理仍仅本地、由LTR01承载。旧被测/已发布树不改写，MTR103原预算不重启；下方更早的当前/候选/待审措辞保留为历史，以本段、任务卡现行字段和H0现行列为准。
```
