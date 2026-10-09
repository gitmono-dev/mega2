139:   | 文件与语义 hunk | owner 与依赖关系 |
140:   |---|---|
141:   | .github/workflows/docker.yml：新增 `REGISTRY_IMAGE`；build job 的 matrix/runner/架构守卫/free-space 步骤、Build Cloud driver 移除、旧 docker job 重命名为 build | FIX-OX-08；该基础结构在 FIX-OX-11 与 FIX-OX-01 的 A/B 中保留 |
142:   | .github/workflows/docker.yml：原 unified hunk `@@ -45,3 +79,1 @@` 必须拆成零 context owner sub-hunks：platform selector 的删除/新增仅归 FIX-OX-08；旧 `push: true` 与 `tags:` 行的删除仅归 FIX-OX-11；manifest 按源 edit ordinal 分别记录两组行，不得将整个 hunk 归给单一卡 | FIX-OX-08 / FIX-OX-11；FIX-OX-11 依赖 FIX-OX-08
143:   | .github/workflows/docker.yml：stable tag 语法守卫（脚本与其执行顺序） | FIX-OX-01 |
144:   | .github/workflows/docker.yml：`Extract metadata` → `Extract image labels`；label metadata 的 image/flavor/tags 与 labels 传递；build 阶段不生成 mutable tag | FIX-OX-11；依赖 FIX-OX-08 |
145:   | .github/workflows/docker.yml：`Build and push` → `Build and push by digest`；逐平台 digest output、digest export 与 artifact upload | FIX-OX-11；依赖 FIX-OX-08 |
146:   | .github/workflows/docker.yml：新增 docker manifest job、build 成功门、concurrency 序列化、tag-channel 计算/标签元数据、artifact 下载、双 digest manifest 合成、平台核验及 mutable-channel 发布 | FIX-OX-01；保留已接受 FIX-OX-08/11 hunks |
147:   | .github/workflows/docker.yml：checkpoint 新增行 8，push 的 `tags: v*` 触发器与 `permissions:` 之间的空白行 | FIX-OX-08 |
148:   | .github/workflows/docker.yml：checkpoint 新增行 12，`env:` 之前的 hunk 起始空白行 | FIX-OX-08 |
149:   | .github/workflows/docker.yml：checkpoint 新增行 15，`REGISTRY_IMAGE` 之后的 hunk 结束空白行 | FIX-OX-08 |
150:   | .github/workflows/docker.yml：checkpoint 新增行 41，stable-tag guard 步骤后的分隔空白行 | FIX-OX-01 |
151:   | .github/workflows/docker.yml：checkpoint 新增行 51，Free space 步骤之后的 hunk 结束空白行 | FIX-OX-08 |
152:   | .github/workflows/docker.yml：checkpoint 新增行 83，既有 `provenance: true` 之后、Export digest 之前的 hunk 起始空白行 | FIX-OX-11 |
153:   | .github/workflows/docker.yml：checkpoint 新增行 100，digest 上传步骤与新增 docker manifest job 之间的空白行 | FIX-OX-11（归前置 digest 上传步骤） |
689: ### Task FIX-OX-08：原生双架构 runner 与平台矩阵（implementation）
690: 
691: **Task type:** `implementation`
692: **Lifecycle / Acceptance:** `pending` / 空
693: **Description:** 自 FIX-OX-01 拆出新 tag 的原生 amd64/arm64 runner、平台矩阵与本机架构守卫；digest 推送和平台 artifact 再拆出 FIX-OX-11，manifest 合成与频道守卫由 FIX-OX-01 承接。与其他发布前 FIX 组成 REL-OX-01，只有 OX-284 在整份计划末尾发布。
694: **Current evidence:** 历史 v0.42.25 tag run 的 `genedna/mono` Cloud driver 额度失败；当前工作树 `.github/workflows/docker.yml:17-99,101-221` 已有原生矩阵，尚无本树新 tag 的远端 D。
695: **Acceptance criteria:**
696: - [ ] AC-1：amd64 row 使用 `linux/amd64` 与 `ubuntu-24.04`。
697: - [ ] AC-2：arm64 row 使用 `linux/arm64` 与 `ubuntu-24.04-arm`。
698: - [ ] AC-3：`build` job 的 `runs-on` 使用本 row 的 `matrix.runner`。
699: - [ ] AC-4：实际 `RUNNER_ARCH` 与本 row 的 `matrix.arch` 不符时 job 非零。
700: - [ ] AC-5：build action 的 `platforms` 只使用本 row 的 `matrix.platform`。
701: - [ ] AC-6：tag build 不再引用已耗尽的 `genedna/mono` Cloud driver。
702: - [ ] AC-7：runner free-space step 只清理由本卡列出的四个预装工具目录，在 checkout 前运行并记录根盘剩余空间；不触碰工作区或变量路径。
703: **Verification:**
704: - [ ] VER-1：`python3 -c 'from pathlib import Path; import re; s=Path(".github/workflows/docker.yml").read_text(); rows=re.findall(r"(?m)^\s+- platform: ([^\n]+)\n\s+runner: ([^\n]+)",s); assert rows==[("linux/amd64","ubuntu-24.04"),("linux/arm64","ubuntu-24.04-arm")],rows; assert "runs-on: ${{ matrix.runner }}" in s; assert "EXPECTED_ARCH: ${{ matrix.arch }}" in s; assert "test \"$RUNNER_ARCH\" = \"$EXPECTED_ARCH\"" in s; assert "platforms: ${{ matrix.platform }}" in s; assert "genedna/mono" not in s; assert "REGISTRY_IMAGE: ${{ vars.DOCKER_USER }}/mega2" in s'`（逐项断言矩阵与本机守卫；digest/artifact 由 FIX-OX-11 验）。
705: - [ ] VER-2：`actionlint -version && actionlint .github/workflows/docker.yml`（保存原件版本、原始诊断和退出码）。仅当本地 v1.7.12 唯一诊断是不识别 `concurrency.queue: max`，依 [GitHub 官方 concurrency.queue 文档](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency) 核对原字段，再运行 `python3 -c 'from pathlib import Path; import tempfile,subprocess,difflib; s=Path(".github/workflows/docker.yml").read_text(); n="      queue: max\n"; assert s.count(n)==1; t=s.replace(n,"",1); f=tempfile.NamedTemporaryFile(mode="w",suffix=".yml",delete=False); f.write(t); f.close(); print("".join(difflib.unified_diff(s.splitlines(True),t.splitlines(True),fromfile="original",tofile="syntax-only"))); raise SystemExit(subprocess.call(["actionlint",f.name]))'`；命令断言原件恰有该行，只改临时副本并打印精确 diff。副本通过只证明其余语法，不记原件 actionlint PASS；OX-284 的新 tag 远端 D 是工作流实际有效性门。
706: - [ ] VER-3：`python3 -c 'from pathlib import Path; import re; s=Path(".github/workflows/docker.yml").read_text(); m=re.search(r"(?ms)^      - name: Free space for the Rust build\n        run: \|\n((?:          [^\n]*\n)+)",s); assert m, "free-space step missing"; assert [x[10:] for x in m.group(1).splitlines()]==["sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/share/powershell","df -h /"]; assert s.index("- name: Free space for the Rust build") < s.index("- name: Checkout")'`（精确核步骤命令白名单与 checkout 前顺序）。
707: **Dependencies:** `DEP-OX-04`。
708: **Implementation write set:** `.github/workflows/docker.yml`、`docs/plan/plan-20260920.md`（本卡状态/证据）。
709: **Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
710: **Rollback mode:** `revert`（撤回未发布 workflow 子提交，旧 tag 失败事实不变）。
711: **Estimated scope:** `S`
712: **Version increment:** `N/A`
713: **Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
714: **C/D coverage from:** `OX-284；继承 D-OX-TAG`。
715: **Granularity:** `type=implementation; axis=原生双架构 runner 与平台矩阵; recovery=撤回未发布 workflow 子提交并保留旧 tag 事实; complete=yes; self-contained=yes; AC=7/8; VER=3/8; landing=1; prod-files=1; scope=S; deps=DEP-OX-04; writeset=no-overlap; release=REL-OX-01 child; split-from=FIX-OX-01; exception=N/A`。
