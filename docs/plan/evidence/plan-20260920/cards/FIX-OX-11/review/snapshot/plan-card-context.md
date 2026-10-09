### Task FIX-OX-11：双架构 digest 推送与平台产物（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 自 FIX-OX-08 拆出原生矩阵构建后的逐平台 digest 推送与 artifact 交接；FIX-OX-01 只消费这两个已标识平台的 digest 合成 manifest。本卡属于 REL-OX-01 计划发布子卡，唯一末尾发布点为 OX-284。
**Current evidence:** 当前工作树 `.github/workflows/docker.yml:17-99,101-221` 已有 digest 输出和平台 artifact；历史 v0.42.25 的 Cloud driver 失败尚无新 tag D，实施日重读矩阵和 artifact 命名。
**Acceptance criteria:**
- [ ] AC-1：amd64 row 只推本平台 digest。
- [ ] AC-2：arm64 row 只推本平台 digest。
- [ ] AC-3：amd64 输出符合 `sha256:<64hex>`。
- [ ] AC-4：arm64 输出符合 `sha256:<64hex>`。
- [ ] AC-5：amd64 digest 以平台专属 artifact 上传。
- [ ] AC-6：arm64 digest 以平台专属 artifact 上传。
- [ ] AC-7：两平台 artifact 名称互不混淆，FIX-OX-01 可按平台精确取回。
- [ ] AC-8：平台 build 以 `provenance: true` 生成可由 manifest job 消费的 OCI index digest；metadata step 只生成 labels/不可变引用，不直接推 mutable tag。
**Verification:**
- [ ] VER-1：`python3 -c 'from pathlib import Path; import re; s=Path(".github/workflows/docker.yml").read_text(); rows=set(re.findall(r"- platform: (linux/(?:amd64|arm64))\n\s+runner: (\S+)\n\s+arch: (\S+)\n\s+slug: (\S+)",s)); checks={"matrix_rows":rows=={("linux/amd64","ubuntu-24.04","X64","amd64"),("linux/arm64","ubuntu-24.04-arm","ARM64","arm64")}, "platform_template":s.count("platforms: "+"$"+"{{ matrix.platform }}")==1, "digest_push":s.count("push-by-digest=true")==1, "digest_format":bool(re.search(r"\[\[ .*sha256:\[0-9a-f\]\{64\}",s)), "artifact_name":s.count("name: digests-"+"$"+"{{ matrix.slug }}")==1, "missing_artifact_error":"if-no-files-found: error" in s, "provenance":s.count("provenance: true")==1, "labels_step":"name: Extract image labels" in s, "latest_disabled":"flavor: latest=false" in s}; print(checks); assert all(checks.values())'`（九项静态断言逐项输出 PASS/FAIL；精确绑定 platform/runner/arch/slug，交换任一平台 slug 必须失败；tag 的真实两架构 digest/artifact 仍由 OX-284 D 判定）。
- [ ] VER-2：`actionlint -version && actionlint .github/workflows/docker.yml`（保存原件版本、原始诊断和退出码）。仅当本地 v1.7.12 唯一诊断是不识别 `concurrency.queue: max`，依 [GitHub 官方 concurrency.queue 文档](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency) 核对原字段，再运行 `python3 -c 'from pathlib import Path; import tempfile,subprocess,difflib; s=Path(".github/workflows/docker.yml").read_text(); n="      queue: max\n"; assert s.count(n)==1; t=s.replace(n,"",1); f=tempfile.NamedTemporaryFile(mode="w",suffix=".yml",delete=False); f.write(t); f.close(); print("".join(difflib.unified_diff(s.splitlines(True),t.splitlines(True),fromfile="original",tofile="syntax-only"))); raise SystemExit(subprocess.call(["actionlint",f.name]))'`；命令断言原件恰有该行，只改临时副本并打印精确 diff。副本通过只证明其余语法，不记原件 actionlint PASS；OX-284 的新 tag 远端 D 是工作流实际有效性门。
**Dependencies:** `FIX-OX-08`。
**Implementation write set:** `.github/workflows/docker.yml`、`docs/plan/plan-20260920.md`（状态与证据）。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（撤回未发布 digest/artifact 子提交，旧 tag 的失败事实不变）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=双平台 digest 推送与 artifact 交接; recovery=撤回未发布的 digest 子提交并保留旧 tag 事实; complete=yes; self-contained=yes; AC=8/8; VER=2/8; landing=1; prod-files=1; scope=S; deps=FIX-OX-08; writeset=序列化于 FIX-OX-08; release=REL-OX-01 child; split-from=FIX-OX-08; exception=N/A`。
