### plan-template.md lines 192-204

   - `Lifecycle`（执行生命周期）：`pending` | `in-progress` | `blocked`（ER-10 的越界故障置此值）| `done`。
   - `Acceptance`（验收状态）：空 | `locally-accepted` | `remote-pending`（仅当本卡有适用的 D 组远端后置门）| `complete`。与 `AGENTS.md` 的完成契约对齐 —— 任何改动只有三门全绿才算 done，因此：
     - `locally-accepted` = 本卡适用的 **A 组 + B 组**门已过，但本卡的 **C 组覆盖**（自行执行或从承接卡继承，见下）尚未取得。此状态下**不得**对外报告「完成 / done」。
     - `remote-pending` = A/B 已过且 C 组覆盖已取得（含一次三门全绿的运行，其被测树状态包含本卡最终变更），但本卡适用或继承的 **D 组**远端后置门尚未全绿。此状态同样**不得**报告完成。
     - `complete` = 本卡的 **A + B** 已过、**C 组覆盖**已取得、且适用或继承的 **D 组**已全绿或按本模板 `EX-*` 规则具名延期。无 D 时 C 覆盖到手即可 `complete`；有 D 时按以下路径处理。
     - 唯一状态转移路径：A/B 通过 → `locally-accepted` → ER-05 review PASS → 取得 C 组覆盖 →（无 D：`complete`；有 D：`remote-pending` → D 全绿或具名 `EX-*` 延期 → `complete`）。已在 C 覆盖之后才批准的延期可从 `remote-pending` 转入 `complete`；延期前已按事实登记的中间状态不得伪造为绿灯。
   - 两者独立取值：`blocked` 卡的 `Acceptance` 可以已是 `locally-accepted` 甚至 `complete`（例如变更已被三门覆盖，但仍卡在外部前置）。`Lifecycle=done` 必须以 `Acceptance=complete` 为前提；`blocked` 必须先回到 `in-progress` 并完成剩余动作才能进入 `done`，**不允许**从 `blocked` 直接标 `done`。计划完成门另要求所有非延后任务都到 `done`（见「完成判据」）。`Granularity` 里的 `complete=yes` 是 G-02 的结构完整性判据，与本字段无关，不可混用。

   **C 组覆盖与执行归属（每张非延后卡都必须取得 C 覆盖，但不都自己执行）:**
   - **独立发布卡（`Release boundary = independent`）与发布点卡（`release` / `family release point` / `plan release point`）**：自行执行完整 C 组门。
   - **`family child` 与 `plan release child`**：逐卡取得 A/B、ER-05 `PASS` 和精确本地提交；不 bump、不构建发布产物、不推送 branch/tag、不创建 GitHub Release。**继承**具名唯一发布点的 C 覆盖——前提是发布点的三门运行其被测最终树包含本子卡全部最终变更；同时继承该发布点适用的 D 组。G-12 的计划级子卡在唯一末尾发布点的 C/D 取得前最多为 `locally-accepted`，不得记 `done/complete`。
   - **`no-release` 卡（`docs` / `audit` / `spike` / `handoff`）**：**继承**任务卡显式声明的承载发布点（或计划收口点）的 C 覆盖与其 D 组；该承载点必须在卡内写明 ID，不得留空。
   - 继承 D 组的卡同样按上述路径处理；具名延期必须明确覆盖本卡及承载发布点。

### plan-template.md lines 273-280


   - D 组**不属于**本地验收，不阻塞 `locally-accepted`，也不改变 ER-05「先本地验收再 review」与 C 组「review 通过后才提交推送」的顺序。
   - 适用的 D 组门全绿或按下文白名单具名延期后，该卡的 `Acceptance` 才能到 `complete`；此前停在中间态 `remote-pending`。延期只调整计划验收口径，不把未运行或未成功的 workflow 记为成功。
   - D 组失败一律**前滚修复**（新提交 / 新版本），不得回退已推送提交；修复卡按 ER-10 的越界规则处理。

   「完成判据」的计划级门是最后一次总检查，不替代每张会推送的卡各自跑过的发布收口门。
5. **ER-05 代码 review 闭环:** 实现和本地验收完成后进行代码 review；review 问题修复后重跑相关验收，直到 review 明确给出 `PASS`。P0/P1 必须关闭，不得以「residual risk 已接受」替代 `PASS`（仅 P2 可由具名责任人书面接受）。
6. **ER-06 文档与兼容同步:** 涉及公开行为的任务必须同步用户文档、开发文档、`config/config.toml` 示例、错误契约、运行时 OpenAPI 证据和测试矩阵。

### plan-template.md lines 637-637

Result 只允许 `PASS` 或 `FAIL`。`FAIL` 必须列出 P0/P1 条目并在下一轮复审关闭；P2 可由具名责任人书面接受为 residual risk，但不改变本轮 `FAIL` 记录（ER-05）。
