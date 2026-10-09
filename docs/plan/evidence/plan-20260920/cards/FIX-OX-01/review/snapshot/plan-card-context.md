### Task FIX-OX-01：新版本 tag 的 Docker 发布执行器（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 在 FIX-OX-08 的原生双架构矩阵和 FIX-OX-11 的逐平台 digest/artifact 上审核 tag manifest 合成与频道守卫。与其他发布前 FIX 组成 REL-OX-01，只有 OX-284 在整份计划最终树发布。
**Current evidence:** 历史 v0.42.25 tag run 使用 `driver: cloud`、`endpoint: genedna/mono` 并因额度失败；当前工作树 `.github/workflows/docker.yml:17-99,101-221` 已是原生双矩阵 digest/manifest，尚无本树新 tag 的远端 D。开工核实该 diff 与 tag 源 SHA。
**Acceptance criteria:**
- [ ] AC-1：`docker` job 显式依赖整个 `build` 矩阵，任一架构失败或取消时不得继续发布 manifest。
- [ ] AC-2：manifest 合成只接受恰好两个合法 sha256 digest。
- [ ] AC-3：为当前 tag 同时产生 `v<version>` 与 `<version>` 两个不可变标签。
- [ ] AC-4：合成后 inspect 的 Linux 架构集合恰为 amd64、arm64。
- [ ] AC-5：只有本 minor 最新 tag 才更新 minor 浮动标签。
- [ ] AC-6：只有全仓最新 tag 才更新 `latest`。
- [ ] AC-7：stable tag guard 只接受 `v<major>.<minor>.<patch>` canonical 版本号，拒绝前导零/缺段/prerelease，并在 checkout、login 与任何 Docker 凭证使用之前运行。
**Verification:**
- [ ] VER-1：`actionlint -version && actionlint .github/workflows/docker.yml`（保存原件版本、原始诊断和退出码）。仅当本地 v1.7.12 唯一诊断是不识别 `concurrency.queue: max`，依 [GitHub 官方 concurrency.queue 文档](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency) 核对原字段，再运行 `python3 -c 'from pathlib import Path; import tempfile,subprocess,difflib; s=Path(".github/workflows/docker.yml").read_text(); n="      queue: max\n"; assert s.count(n)==1; t=s.replace(n,"",1); f=tempfile.NamedTemporaryFile(mode="w",suffix=".yml",delete=False); f.write(t); f.close(); print("".join(difflib.unified_diff(s.splitlines(True),t.splitlines(True),fromfile="original",tofile="syntax-only"))); raise SystemExit(subprocess.call(["actionlint",f.name]))'`；命令断言原件恰有该行，只改临时副本并打印精确 diff。副本通过只证明其余语法，不记原件 actionlint PASS；OX-284 的新 tag 远端 D 是工作流实际有效性门。
- [ ] VER-2：`rg -n 'needs: build|needs.build.result|imagetools create|imagetools inspect|minor_latest|latest=' .github/workflows/docker.yml && python3 -c 'from pathlib import Path; import re; s=Path(".github/workflows/docker.yml").read_text(); m=re.search(r"(?m)^    concurrency:\n((?:^      .*(?:\n|$))*)",s); assert m, "manifest concurrency block missing"; b=m.group(1); assert re.search(r"(?m)^      group: mega2-docker-publish$",b); assert re.search(r"(?m)^      queue: max$",b)'`（结构性核对 docker job 的 concurrency block 与 max queue；不依赖跨行 grep pattern；新 tag 的实际构建/推送由 OX-284 D 验收）。
- [ ] VER-3：`python3 -c 'from pathlib import Path; import re; s=Path(".github/workflows/docker.yml").read_text(); m=re.search(r"if \[\[ ! \"\$REF_NAME\" =~ (.+) \]\]; then",s); assert m, "stable tag guard missing"; p=re.compile(m.group(1)); assert all(p.fullmatch(x) for x in ("v0.0.0","v1.2.3","v10.20.30")); assert all(not p.fullmatch(x) for x in ("v01.2.3","v1.2","v1.2.3-rc.1","1.2.3")); assert s.index("- name: Validate stable release tag") < s.index("- name: Checkout") < s.index("- name: Log in to Docker Hub")'`（对 workflow 中实际 Bash regex 做正反例并核对凭证前执行顺序）。
**Dependencies:** `FIX-OX-11`、`DEP-OX-04`。
**Implementation write set:** `.github/workflows/docker.yml`、`docs/plan/plan-20260920.md`（本卡状态/证据）。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（以新提交恢复既有工作流并保留已发 tag 的历史证据）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=新版本 tag manifest 合成与频道守卫; recovery=撤销未发布 workflow 子提交并保留旧 tag 事实; complete=yes; self-contained=yes; AC=7/8; VER=3/8; landing=1; prod-files=1; scope=S; deps=FIX-OX-11,DEP-OX-04; writeset=序列化于 FIX-OX-11; release=REL-OX-01 child; split-from=N/A; exception=N/A`。

### Task FIX-OX-02：既有 MST2 bounded snapshot fixture 基线修复（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 修复 snapshot_chunks_bounded_tests.rs 两条 rooted fixture 的构造时序并建立 snapshot_content_tests.rs options scaffold（含 InitialFact、chunk faults 与 lease 选项）；正向 body/fact 必须匹配，batch 用例保留原 413 预算与 404 后续路径拒绝。该 helper scaffold 由本卡唯一拥有，FIX-OX-05/09/12 只消费。
