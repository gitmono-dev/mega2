# Debug：mega2 测试进程被 SIGKILL 的诊断与监控方案

> 本文记录 mega2 测试在开发主机（omarchy-lenovo）上被 SIGKILL 的诊断结论，
> 以及用来抓“凶手”的监控方案（含完整源码）。适用于在本主机上跑的
> `cargo test`（lib / bins / IT）与 `cargo build`。
>
> 创建日期：2026-09-20

## 1. 症状

- 用 `cargo test --lib -- --test-threads=1` 串行运行大量测试时，测试二进制会在中途被 **SIGKILL（信号 9）** 杀掉。
- cargo 报告：`process didn't exit successfully: .../target/debug/deps/mega2_core-* --test-threads=1 (signal: 9, SIGKILL: kill)`，**LIB_RC:101**。
- 被杀的测试**不固定**：`un24_merge_matrix` 只是其中一次（它本身很轻量，历史日志中 16 次全部通过）；
  其他几轮死在 `tp01_migration_up...`、`events_unknown_kind_kept` 等测试上，与串行运行时累积的内存有关。
- **只有测试二进制被杀**：外层进程（`cargo` 和 agent 运行环境的沙箱 bash）都存活，本轮会继续跑完 bins（`BINS_RC:0`）和集成测试（IT）。
- 集成测试阶段（`--tests -j 16 -- --test-threads=8`）通常能跑完；问题集中在 lib 串行阶段。

## 2. 诊断结论（截至 2026-09-20）

| 信号 | 结果 |
|---|---|
| 系统内存 | 93 Gi 内存 + 186 Gi 交换空间（zram 93.5G + 交换文件 93.5G），可用内存通常在 75 Gi 以上 |
| 内核 OOM 杀进程（`journalctl -k`，近 5 天） | **零记录** |
| cgroup `memory.events` 的 oom_kill | **全部为 0**；测试进程所在的 `user@1000.service`、`app.slice` 和 Hyprland scope 的 `memory.max` 均为 `max` |
| systemd-oomd | 从未记录过杀进程（已禁用；PSI 也已关闭） |
| `vm.overcommit_memory` | 0（默认的启发式策略），CommitLimit 为 245G，足够 |

**结论：不是系统层面的 OOM。** 而是有进程从用户态对测试二进制发出了**外部 `kill -9`**，
最可能来自 agent 运行环境（Cursor 沙箱）的内存看门狗，或自定义的 cgroup 实验脚本
（计划文件名带 `cgroup` / `cgroup2` / `cgroup3` 的那几次实验）。
已安装 auditd 规则（见 §4），下一次出现 SIGKILL 时即可抓到发送者身份。

## 3. 监控方案：oom-monitor.service

这是一个被动监视进程，**不依赖 PSI 和 systemd-oomd**（两者都已关闭）。它监视四个独立信号，并定期输出心跳：

1. **内核 OOM**：轮询 `journalctl -k`，一旦出现 `Out of memory` 或 `Killed process`，立即记录完整的内核日志。
2. **cgroup OOM**：扫描所有 cgroup 的 `memory.events`，检查 `oom_kill` 计数是否增加（包括 docker scope 和 systemd-run scope）。
3. **构建进程退出检测**：监视 `cargo`、`rustc`、带 `--test-threads` 参数以及 `target/debug/deps` 下的进程（包括 `mega2_core-*`），
   PID 一消失就抓取快照，连不留任何记录的外部 `kill -9` 也能捕捉到。
4. **系统日志 SIGKILL 扫描**：每 5 分钟扫描一次 systemd 杀进程和 SIGKILL 的记录。

每 15 秒记录一行心跳（已用内存、可用内存、交换空间、zram、RSS 最高的进程、构建进程数）。
日志位于 `~/oom-monitor/oom-monitor.log`，超过 8 MiB 自动轮转。

### 安装

```bash
install -d ~/oom-monitor
# 将下方“源码”的 oom-monitor.sh 存入 ~/oom-monitor/oom-monitor.sh
chmod +x ~/oom-monitor/oom-monitor.sh

systemd-run --user --unit=oom-monitor --collect \
  --property=Restart=on-failure \
  --description="OOM/SIGKILL watcher for build&test" \
  /bin/bash ~/oom-monitor/oom-monitor.sh

systemctl --user status oom-monitor      # 确认 active
tail -f ~/oom-monitor/oom-monitor.log    # 观察心跳
```

可用环境变量覆盖默认值：`OOM_MONITOR_DIR`（日志目录）、`OOM_MONITOR_INTERVAL`（轮询间隔秒数，默认 15）。

## 4. 监控方案：auditd 规则（抓 kill -9 的发送者）

auditd 记录“谁对谁发了 SIGKILL”：发送者的 PID、UID、进程名、可执行文件和父进程，被杀进程的 PID 和进程名，以及时间戳。

> 注意：audit 规则**没有 `sig` 字段**，需要按系统调用的参数位置过滤：
> `kill(pid,sig)=a1`、`tkill(tid,sig)=a1`、`tgkill(tgid,tid,sig)=a2`、`pidfd_send_signal(pidfd,sig,info,flags)=a1`。

### 安装（一次性，需 root）

```bash
sudo bash ~/oom-monitor/setup-audit.sh
```

脚本会依次：写入 `/etc/audit/rules.d/oom-kill.rules`（持久规则）、添加 systemd drop-in 配置
（让 auditd 每次启动时重新加载规则）、执行 `systemctl enable auditd`、加载规则，最后发送一次测试用的 `kill -9` 自检。
> auditd.service 设置了 `RefuseManualStop=yes`，`systemctl restart` 会被拒绝，所以脚本改为直接加载规则。

生效的规则如下：

```
-a always,exit -F arch=b64 -S kill -F a1=9 -k oomkill
-a always,exit -F arch=b64 -S tkill -F a1=9 -k oomkill
-a always,exit -F arch=b64 -S tgkill -F a2=9 -k oomkill
-a always,exit -F arch=b64 -S pidfd_send_signal -F a1=9 -k oomkill
```

### 查找发送者

```bash
sudo bash ~/oom-monitor/check-audit.sh                      # 今天
sudo bash ~/oom-monitor/check-audit.sh --since "1 hour ago" # 指定时间段
```

记录示例（`ausearch -k oomkill -i`）：

```
type=OBJ_PID  : opid=975621 ocomm=bash            ← 被杀进程
type=SYSCALL  : syscall=kill a1=SIGKILL exit=0
               pid=975473 comm=bash exe=/usr/bin/bash ppid=975472 auid=genedna
               key=oomkill                         ← 发送者身份
```

## 5. 排障流程与判定表

1. 监控日志中出现事件区块（`KERNEL OOM EVENT`、`CGROUP OOM_KILL`、`BUILD PROC EXIT` 或 `SIGKILL RECORDS`）。
2. 按对应的时间点运行 `ausearch -k oomkill`。
3. 按下表判断是哪类原因杀掉了进程：

| 现象 | 判定 |
|---|---|
| `journalctl -k` 中有 `Out of memory: Killed process`，且该 cgroup 的 `oom_kill` 计数增加 | 内核 OOM killer |
| 某个 cgroup 的 `memory.max` 不是 `max`，且 `oom_kill` 计数大于 0 | 触发了 cgroup 内存上限 |
| 内核、cgroup 和 systemd 都没有记录，但 `ausearch -k oomkill` 有记录 | 外部 `kill -9`（发送者身份见 audit 记录） |
| 进程被杀，但 audit 也没有记录 | 有进程写入了 `cgroup.kill`（不留系统调用记录）；监控快照会显示当时的 cgroup 状态 |

> 已知限制：audit 规则从加载时起才开始记录，无法追溯之前的杀进程事件；
> 通过 `cgroup.kill` 杀进程不经过 kill 系统调用，audit 抓不到（但监控的 BUILD PROC EXIT 和快照仍会触发）。

## 6. 源码

### 6.1 `oom-monitor.sh`

```bash
#!/usr/bin/env bash
# oom-monitor.sh — 面向编译和测试负载的被动 OOM / SIGKILL 监视脚本
#
# 由 ~/oom-monitor/sync-docs.sh 自动同步到 libra 和 mega2 的文档
#（debug-oom-kill-monitoring.md §6.1），由 oom-sync.path 单元在本文件变更时触发。
#
# 相互独立的信号（不依赖 PSI 和 systemd-oomd）：
#   1. 内核 OOM 消息（轮询 journalctl -k）              — 真正的内核 OOM killer
#   2. cgroup v2 memory.events 的 oom_kill 计数         — 容器 / scope 内存上限
#   3. 构建 / 测试进程退出监视（cargo/rustc/--test-threads/target/deps）
#      — 任何退出都会触发，包括没有内核记录的用户态 kill -9
#   4. 系统日志 SIGKILL / systemd 杀进程扫描            — systemd 层面的杀进程
# 另有滚动的内存心跳，便于查看每次杀进程前后的状态。
#
# 可覆盖的环境变量：OOM_MONITOR_DIR、OOM_MONITOR_INTERVAL
set -u

BASE="${OOM_MONITOR_DIR:-$HOME/oom-monitor}"
LOG="$BASE/oom-monitor.log"
INTERVAL="${OOM_MONITOR_INTERVAL:-15}"
MAXLOG="${OOM_MONITOR_MAXLOG:-8388608}"   # 超过 8 MiB 轮转
KILL_SCAN_EVERY=20                         # 每 N 轮扫描一次系统日志中的 SIGKILL（约 5 分钟）
export PATH=/usr/bin:/bin:/usr/local/bin

mkdir -p "$BASE"

now()  { date '+%F %T'; }
log()  { printf '%s %s\n' "$(now)" "$*" >> "$LOG"; }

rotate() {
  [ -f "$LOG" ] || return 0
  [ "$(stat -c%s "$LOG")" -lt "$MAXLOG" ] && return 0
  mv "$LOG" "$LOG.1"
  log "=== rotated ==="
}

last_ts() { date '+%Y-%m-%d %H:%M:%S'; }

# --- 检测到杀进程时记录详细快照 ---
snapshot() {
  local reason="$1" from="$2"
  rotate
  log "===== DETAILED SNAPSHOT ($reason) @ $(now) ====="
  log "uptime: $(uptime -p)  load: $(cut -d' ' -f1-3 /proc/loadavg)"
  { echo "--- free ---";      free -h; }                                         >> "$LOG" 2>&1
  { echo "--- swap ---";      swapon --show; }                                   >> "$LOG" 2>&1
  { echo "--- zram mm_stat (orig_data compr_data mem_used mem_limit mem_used_max) ---"
    awk '{printf "%d %d %d %d %d\n",$1,$2,$3,$4,$5}' /sys/block/zram0/mm_stat; } >> "$LOG" 2>&1
  { echo "--- top 20 RSS ---"; ps -eo pid,user,rss,comm --sort=-rss | head -21; } >> "$LOG" 2>&1
  { echo "--- build/test procs ---"
    ps -eo pid,ppid,rss,etime,args | grep -E 'cargo|rustc|--test-threads|/target/debug/deps' | grep -v grep; } >> "$LOG" 2>&1
  { echo "--- all cgroup oom counters (nonzero only) ---"
    find /sys/fs/cgroup -name memory.events 2>/dev/null | while read -r f; do
      oom=$(awk '/^(oom|oom_kill|oom_group_kill) /{printf "%s=%s ",$1,$2}' "$f" 2>/dev/null)
      [ -n "$oom" ] && [ "$oom" != "oom=0 oom_kill=0 oom_group_kill=0 " ] && echo "$f : $oom"
    done; } >> "$LOG" 2>&1
  { echo "--- journal delta since $from (kill/oom/stop related) ---"
    journalctl --since "$from" --no-pager 2>/dev/null \
      | grep -iE 'systemd\[[0-9]+\].*(kill|stop|deactivat)|oom|Killed process|Killing process|signal 9|sigkill|memory cgroup' \
      | tail -25; } >> "$LOG" 2>&1
  log "===== end snapshot ====="
}

# --- 精简心跳行（每轮一次） ---
heartbeat() {
  read -r mu ma < <(awk '/MemTotal|MemAvailable/{printf "%d ", $2}' /proc/meminfo)
  read -r su st < <(awk '/^SwapTotal|^SwapFree/{printf "%d ", $2}' /proc/meminfo)
  zram_mem=$(awk '{print int($3/1024/1024)}' /sys/block/zram0/mm_stat 2>/dev/null)
  top1=$(ps -eo rss,comm --sort=-rss | awk 'NR==2{printf "%s(%dMB)",$2,int($1/1024)}')
  nbuild=$(ps -eo args | grep -cE 'cargo|rustc|--test-threads' | tr -d ' ')
  log "HB used=$(( (mu-ma)/1024 ))MB avail=$((ma/1024))MB swap_used=$(( (su-st)/1024 ))MB zram_mem=${zram_mem}MB top=${top1} build_procs=${nbuild}"
}

# ===== 初始化 =====
rotate
log "=== oom-monitor started (pid $$, interval=${INTERVAL}s, psi=off, oomd=off) ==="

# 记录 cgroup oom_kill 计数的基线
declare -A prev
while IFS= read -r f; do
  v=$(awk '/^oom_kill/{print $2}' "$f" 2>/dev/null)
  [ -n "$v" ] && prev["$f"]="$v"
done < <(find /sys/fs/cgroup -name memory.events 2>/dev/null)

# 记录构建 / 测试进程的基线（pid -> "启动时间|命令行"）
declare -A bprocs
scan_procs() {
  local out pid rest lstart args
  # cargo / rustc 按可执行文件名（comm）匹配；测试进程按实际调用匹配：
  # 带 --test-threads=<n> 参数，或是 /target/debug/deps/ 下的二进制
  out=$(ps -eo pid=,lstart=,comm=,args= 2>/dev/null \
    | awk '$7=="cargo"||$7=="rustc"||$0~/--test-threads=[0-9]+/||$0~/\/target\/debug\/deps\//{print}')
  bprocs=()   # 清空旧条目（已退出的 pid 必须从跟踪集合中消失）
  [ -z "$out" ] && return 0
  while IFS= read -r line; do
    read -r pid rest <<< "$line"   # 处理行首空白；pid 取第一个字段
    lstart=$(awk '{for(i=1;i<=6&&i<=NF;i++)printf "%s%s",$i,(i<6?" ":"");exit}' <<< "$rest")
    args=$(awk '{for(i=7;i<=NF;i++)printf "%s%s",$i,(i<NF?" ":"");exit}' <<< "$rest")
    [ -n "$pid" ] && bprocs["$pid"]="${lstart}|${args}"
  done <<< "$out"
}
scan_procs

last_ts_k="$(last_ts)"
last_ts_j="$(last_ts)"
cycle=0

trap 'log "=== oom-monitor stopped ==="' EXIT

# ===== 主循环 =====
while true; do
  cycle=$((cycle+1))

  # 1) 上次轮询以来新增的内核 OOM 消息
  new_k=$(journalctl -k --since "$last_ts_k" -o short-iso --no-pager 2>/dev/null)
  last_ts_k="$(last_ts)"
  if printf '%s' "$new_k" | grep -qiE 'Out of memory|oom-kill|Killed process|oom_reaper|memory cgroup out of memory'; then
    rotate
    log "===== KERNEL OOM EVENT ====="
    printf '%s\n' "$new_k" >> "$LOG"
    log "===== end kernel oom ====="
    snapshot "kernel-oom" "$last_ts_k"
  fi

  # 2) cgroup oom_kill 计数的增量
  while IFS= read -r f; do
    v=$(awk '/^oom_kill/{print $2}' "$f" 2>/dev/null)
    [ -z "$v" ] && continue
    if [ "${prev[$f]:-0}" -gt 0 ] && [ "$v" -gt "${prev[$f]}" ]; then
      log "CGROUP OOM_KILL: $f now=$v prev=${prev[$f]}"
      snapshot "cgroup-oom-kill:$f" "$last_ts_k"
    fi
    prev["$f"]="$v"
  done < <(find /sys/fs/cgroup -name memory.events 2>/dev/null)

  # 3) 构建 / 测试进程退出监视（能捕捉不留记录的用户态 kill -9）
  declare -A old
  for k in "${!bprocs[@]}"; do old["$k"]="${bprocs[$k]}"; done
  scan_procs   # 用当前进程集合重新填充全局 bprocs
  for pid in "${!old[@]}"; do
    if [ -z "${bprocs[$pid]:-}" ]; then
      was="${old[$pid]}"
      log "BUILD PROC EXIT: pid=$pid was=[$was]"
      # 测试 / deps 二进制退出时记录完整快照；rustc / cargo 退出只记一行
      #（除非这段时间的系统日志里出现杀进程或 OOM 迹象）
      case "$was" in
        *rustc*|*cargo*)
          if journalctl --since "$last_ts_j" --no-pager 2>/dev/null \
             | grep -qiE 'Killed process|Killing process|signal SIGKILL|out of memory|oom-kill'; then
            snapshot "build-proc-exit(abnormal?):pid=$pid" "$last_ts_j"
          fi ;;
        *)
          snapshot "build-proc-exit:pid=$pid" "$last_ts_j" ;;
      esac
    fi
  done
  unset old

  # 4) 定期扫描系统日志中的 SIGKILL / systemd 杀进程记录（排除本监控单元自身）
  if [ $((cycle % KILL_SCAN_EVERY)) -eq 0 ]; then
    hits=$(journalctl --since "$last_ts_j" --no-pager 2>/dev/null \
      | grep -iE 'Killing process .* with signal SIGKILL|signal SIGKILL|exit code=.*killed|killed by signal|Sent signal SIGKILL' \
      | grep -v 'oom-monitor' | tail -10)
    last_ts_j="$(last_ts)"
    if [ -n "$hits" ]; then
      rotate
      log "===== SIGKILL RECORDS (journal) ====="
      printf '%s\n' "$hits" >> "$LOG"
      log "===== end sigkill ====="
      snapshot "journal-sigkill" "$last_ts_j"
    fi
  fi

  heartbeat
  sleep "$INTERVAL"
done
```

### 6.2 `setup-audit.sh`

```bash
#!/usr/bin/env bash
# setup-audit.sh — 安装 auditd 规则，抓取是谁发送了 SIGKILL（kill -9）。
# 以 root 运行：sudo bash ~/oom-monitor/setup-audit.sh
set -euo pipefail

RULES_FILE=/etc/audit/rules.d/oom-kill.rules
DROPIN=/etc/systemd/system/auditd.service.d/oom-rules.conf

echo "==> writing persistent audit rules: $RULES_FILE"
cat > "$RULES_FILE" <<'EOF'
## oom-monitor：抓取经 kill/tkill/tgkill/pidfd_send_signal 发出的每一个 SIGKILL（信号 9）
## 注意：audit 规则没有 `sig` 字段，按系统调用的参数位置过滤：
##   kill(pid,sig)=a1  tkill(tid,sig)=a1  tgkill(tgid,tid,sig)=a2  pidfd_send_signal(pidfd,sig,info,flags)=a1
-a always,exit -F arch=b64 -S kill -F a1=9 -k oomkill
-a always,exit -F arch=b64 -S tkill -F a1=9 -k oomkill
-a always,exit -F arch=b64 -S tgkill -F a2=9 -k oomkill
-a always,exit -F arch=b64 -S pidfd_send_signal -F a1=9 -k oomkill
EOF

echo "==> writing systemd drop-in (reload rules on auditd start): $DROPIN"
mkdir -p "$(dirname "$DROPIN")"
cat > "$DROPIN" <<'EOF'
[Service]
ExecStartPost=-/sbin/auditctl -R /etc/audit/rules.d/oom-kill.rules
EOF

echo "==> enabling + starting auditd"
systemctl enable auditd
if ! systemctl is-active --quiet auditd; then
  systemctl start auditd
fi
systemctl is-active auditd

# 注意：auditd.service 设置了 RefuseManualStop=yes，`systemctl restart` 会被拒绝。
# auditd 已在运行，直接用 auditctl 加载规则即可（无需重启）。
# 上面的 ExecStartPost drop-in 会在以后每次（重新）启动或开机时重新加载规则。

echo "==> loading rules now"
auditctl -D   # 清空当前所有规则（本机原本没有规则）
auditctl -R "$RULES_FILE"

echo "==> active rules:"
auditctl -l

echo "==> self-test: sending SIGKILL to a throwaway process"
sleep 1000 & T=$!
kill -9 "$T" 2>/dev/null || true
sleep 1
echo "==> ausearch result (key=oomkill, last 5 min):"
ausearch -k oomkill -ts recent -i 2>&1 | tail -40

echo
echo "DONE. To inspect who killed a build/test process later:"
echo "  sudo bash ~/oom-monitor/check-audit.sh"
```

### 6.3 `check-audit.sh`

```bash
#!/usr/bin/env bash
# check-audit.sh — 显示是谁发送了 SIGKILL，默认查今天（或用 --since 指定起始时间）。
# 以 root 运行：sudo bash ~/oom-monitor/check-audit.sh [--since <date>]
set -euo pipefail
SINCE="${2:-today}"
sudo ausearch -k oomkill -ts "$SINCE" -i 2>&1 | tail -100
```
