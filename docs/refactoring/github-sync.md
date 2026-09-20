# GitHub outbound sync

Contract for unidirectional monorepo-path → GitHub infrastructure
(`plan-20260916`). This file is created by GS-14 and extended by later cards.

## 计划守卫

Three scripts share one exit-code contract: `0` pass, `1` decision failure
(out of allowlist / dangling doc ref), `2` command or input error. Guard
output prints paths and verdicts only — never file contents (ER-11).

### `scripts/guard/baseline.sh <out>` (GS-14)

Saves a content-plus-metadata baseline for every dirty path reported by
`libra status --short` (`XY path`). Only a status column containing `R`
splits `old -> new` into two paths; a literal ` -> ` in any other status
is one path. C-quoted paths (`"…\nnn…"`, including `\"`) are decoded
before fingerprinting. A decoded path that contains a tab or newline is
unrepresentable in the line format and exits `2`. Output is sorted by
the decoded path. Each line is:

```
<kind>|<mode>|<fingerprint>  <path>
```

| kind | mode | fingerprint |
|---|---|---|
| `file` | octal permission (`stat -c '%a'`) | SHA-256 of file bytes |
| `symlink` | `-` | `readlink` target |
| `absent` | `-` | `-` |
| `unsupported` | `-` | `-` (directory, FIFO, device, …) |

`libra status`, `sha256sum`, `stat`, `readlink`, `sort`, or writing
`<out>` failing makes the script exit `2`. Isolated fixtures:
`tests/fixtures/guard/run_baseline_shape_cases.sh` and
`run_baseline_failure_cases.sh`.

### `scripts/guard/allowlist.sh <baseline> <allowed-path>...` (GS-22)

Compares the union of baseline paths and current dirty paths. Allowed
paths may change. Any other path whose current fingerprint differs from
the baseline exits `1`. Rename source and destination are judged
separately. `kind=unsupported` is always out of allowlist, even when the
path was passed as allowed. Missing baseline, empty allowlist, usage
error, or tool failure (`libra` / `sha256sum` / `stat` / `readlink` /
`grep` / `sort`) exits `2`. Isolated fixtures:
`tests/fixtures/guard/run_allowlist_cases.sh` and
`run_allowlist_failure_cases.sh`.

A baseline or live symlink record whose target contains two consecutive
spaces is ambiguous in the line format and exits `2`.

Callers must pass that card’s exact product paths — never a wide
directory such as `docs`.

### `scripts/guard/docrefs.sh <markdown-file>...` (GS-19)

Extracts references matching `docs/` plus ASCII letters, digits, `_`,
`.`, `/`, `-`, then `.md`, using `rg -o --no-filename` (no multi-file
prefixes; globs, placeholders, and CJK punctuation are not path
characters). Dangling refs exit `1`. Empty match set is success (`0`).
`rg` exit `>1`, `sort` failure, missing input, or no arguments exit
`2`. Isolated fixtures:
`tests/fixtures/guard/run_docrefs_cases.sh` and
`run_docrefs_failure_cases.sh`.

## 配置面

`[github_sync]` is a first-class `Config` field (GS-03). The section is
optional: omitting it loads `GithubSyncConfig::default()`, which has
`enabled = false` and empty strings / no bindings. `ssh_key_ref` is a
string path only — this card does not resolve secrets.

| Type | Fields |
|---|---|
| `GithubSyncConfig` | `enabled`, `ssh_host`, `ssh_user`, `ssh_host_key`, `ssh_key_ref`, `bindings` |
| `GithubSyncBinding` | `id`, `path`, `remote` |

Whitelist (`github_sync` / `github_sync.bindings`), restart-required
reload classification, commented `config/config.toml` plus `config init`
template, and bilingual README links are GS-23. Binding-entry checks
remain GS-13. No runtime sync runs from this schema.

## 全局前置门

Startup `Config::validate` rejects combinations that cannot run
(plan-20260916 GS-04). `enabled=false` still parses and namespace-checks
a non-empty `ssh_key_ref`; vault values are not resolved (GS-05).

| When | Rejected if |
|---|---|
| `enabled=true` | `git.push_auth` is unset |
| `enabled=true` | `bindings` is empty |
| `enabled=true` | `monorepo.object_format` is not `sha1` |
| `enabled=true` | `ssh_host_key` is empty |
| `enabled=true` | `ssh_key_ref` fails `validate_config_secret_ref` under `config/<profile>/github_sync/ssh_key` |
| `enabled=false` | non-empty `ssh_key_ref` fails SecretRef parse or the same namespace |

Error text names the field or gate; it never echoes the `ssh_key_ref` URI.

## binding 合法性矩阵

Startup `Config::validate` checks every `[github_sync.bindings]` row
(plan-20260916 GS-13). Duplicate diagnostics name both 1-based indexes.

| Field | Rejected if |
|---|---|
| `id` | not 1..32 characters of `[A-Za-z0-9_-]`, or not unique |
| `path` | not a canonical absolute path (`.`, `..`, `//`, trailing `/`) |
| `path` | not under `/project` by component boundary (`/projectX` fails) |
| `path` | equal to `monorepo.import_dir` or under it (import_dir compared after normalizing `.` / `..`) |
| `path` | not unique |
| `remote` | not `<owner>/<repo>` with each side `^[A-Za-z0-9_-][A-Za-z0-9_.-]*$` and not `.` / `..` |
| `remote` | not unique |

## 金钥生命周期

Startup `AppContext` loads or generates one Ed25519 SSH key when
`[github_sync] enabled=true` (plan-20260916 GS-05). The private key is
stored in OpenSSH format at `ssh_key_ref` and held in process memory.

| When | Behavior |
|---|---|
| `enabled=true`, vault has no key | generate Ed25519, write OpenSSH private key, hold |
| `enabled=true`, vault already has a key | load it, do not overwrite, hold |
| `enabled=false` | do not generate, do not write vault |

Algorithm is Ed25519 only. Deleting the vault entry and restarting
generates a new key (public key differs).

## 自举的并发与故障语义

When `[github_sync] enabled=true` and the vault entry is missing, startup
takes a Redis RedLock (`mega2:github_sync:ssh_key:init`, same mechanism
as `ensure_server_signing_key`) before generating. The vault value is
re-read inside the lock so concurrent replicas converge on one key.

| When | Behavior |
|---|---|
| vault already has a valid key | load it on the fast path; no lock |
| vault has no key | acquire RedLock, re-read, generate only if still missing |
| vault read or write error | fail closed; do not install a process hold |
| vault value is present but not a valid OpenSSH Ed25519 key | fail closed; do not overwrite |

### 自举残余风险

If the lock is lost (TTL expiry or Redis partition) while a replica is
still generating, a second replica may also generate. The last writer
wins; a process hold may then diverge from vault until restart. This
window is not closed here (vault has no compare-and-set). Residual-risk
questions are frozen at plan-20260916 GS-28 Q8. The operator repair
is: stop every replica, delete the vault entry, restart one replica to
bootstrap, start the rest, replace the GitHub machine-account key with
the new public key.

## 公钥获取方式

When `[github_sync] enabled=true`, startup logs the OpenSSH public key
as a paste-ready line. The private key is not written to logs, error
text, or `Debug`.

| Branch | Log |
|---|---|
| generated | machine-readable `github_sync_ssh_key_generated` / 「生成」; full `ssh-ed25519 … mega2-github-sync` line; hint to add the key to the GitHub machine account |
| loaded | machine-readable `github_sync_ssh_key_loaded` / 「载入」; the same public-key line as generation |

### 运维指引

1. Set `enabled=true` and start one replica.
2. Copy the `ssh-ed25519 AAAA… mega2-github-sync` line from the startup log.
3. Add that public key to the GitHub machine account.
4. Further replicas load the same vault key and log the same public line.

## 传输与主机金钥钉住

Outbound SSH reads the process hold from GS-05, connects to
`[github_sync].ssh_host` / `ssh_user`, and authenticates with that
Ed25519 key. `ssh_host` is `host` or `host:port` (default port 22).

The server host key is compared to `ssh_host_key` as OpenSSH public-key
bytes (exact trimmed encoding, or the parsed key-material bytes when the
configured line carries a comment). A mismatch, an unparseable pin, or a
certificate host key aborts **before** public-key authentication.
Non-Ed25519 client keys are rejected with an actionable `auth` error.

Failures carry `stage=ssh_connect|host_key|auth`. The TCP plus handshake
plus public-key authentication budget is 15 seconds; a timeout is
`ssh_connect`, not an unbounded wait. The russh session also has a 15
second inactivity bound so a cancelled or stalled handshake cannot leak
a background task or socket.

Loopback evidence is `tests/integration_github_sync.rs`
(`ssh_connect_authenticates`). Production GitHub is `DEFER-GS-08`.

## receive-pack 协议与能力协商

After the SSH session is authenticated, github_sync runs
`git-receive-pack '<owner>/<repo>.git'` (single-quoted; a remote that
contains `'` is rejected). The advertisement is parsed as pkt-lines
until flush. `refs/heads/main` becomes the remote tip; if that ref is
absent the tip is 40 zero hex digits. Capabilities are taken from the
NUL-terminated first ref line.

`report-status` is required. If it is missing, the client aborts with an
error that names `report-status` and writes no command or pack bytes.

Loopback evidence is `loopback_advertise`. Command construction and pack
write are GS-24. Production GitHub is `DEFER-GS-08`.

## 请求构造与降级

After the advertisement is accepted, the client writes one receive-pack
command pkt-line and a flush, then streams the packfile.

The command payload is
`<old> <new> refs/heads/main\0 <capabilities>` — NUL, then a leading
space before the capability list, and **no** trailing LF. The pkt-line
is followed immediately by `0000`.

`report-status` is always requested (GS-08 already required it).
`side-band-64k` is requested only when the advertisement declared it;
otherwise the write result sets `missing_sideband`. Pack bytes are
written in at most 16 KiB windows; a larger caller chunk is split. Live
residency is the command frame plus one window, not the full pack.

Loopback evidence is `loopback_receive_pack`. Report-status termination
is GS-15.

## 报告层终止契约

Success requires all three of `unpack ok`, `ok refs/heads/main`, and a
terminating flush. `ng refs/heads/main <reason>` is a ref rejection and
keeps the reason string. `unpack <error>` that is not `ok` is
`report_unpack` and keeps the server error text. A missing or
half-finished report is `report_incomplete`. A closed connection before
flush is `report_disconnect`. Side-band channel 3 is `report_fatal` and
stops without waiting for later report lines.

When `side-band-64k` is negotiated, channel 1 carries the inner
pkt-line `report-status` stream and may split a pkt-line across
packets. Channel 2 is collected into the diagnostic budget. Channel 3
is fatal immediately and also counts toward that budget. An outer
multiplex flush without a complete inner report is
`report_incomplete`, not a disconnect.

SSH transport termination is below. Deadlines are GS-20. Diagnostic
byte budgets are in 「诊断容量边界」.

## 传输层终止事件

SSH `stderr` (`SSH_EXTENDED_DATA_STDERR`, RFC 4254 type code 1) is
collected into diagnostics as `ssh_stderr`. Any `exit-signal` is
`ssh_exit_signal`. A non-zero `exit-status` is `ssh_exit_status`. Both
of those failures outrank a complete successful report-status.

Report-layer failure (including side-band channel 3 fatal) returns
immediately. Exit events are recorded and the reader keeps draining
stdout until EOF/close, because OpenSSH may send `exit-status` before
the last report bytes.

Final success is report-layer success plus an explicit `exit-status = 0`.
EOF or close without `exit-status` is `ssh_exit_missing` and is never
success; the wait is bounded by the exit deadline below.

## 时间边界

Four independent receive-pack deadlines live on `[github_sync]`:

| Field | Default | Stage on expiry |
|---|---|---|
| `advertise_timeout_seconds` | 30 | `advertise` |
| `send_timeout_seconds` | 300 | `send` |
| `report_timeout_seconds` | 60 | `report` |
| `exit_timeout_seconds` | 15 | `report` |

Each expiry cancels the in-flight future, marks the receive-pack
aborted, and shuts down the SSH TCP socket (SO_LINGER 0 + close/RST)
so queued writes cannot resume when the peer reads again. The SSH
session inactivity bound is at least the max of these four fields
(and the 15s connect timeout); connect itself is still cancelled at
15s and aborts the TCP fd so a failed handshake cannot leak.
`exit_timeout_seconds` starts when report-status parses successfully
or stdout closes, then covers the SSH exit-status/exit-signal wait
and any remaining stdout drain. Zero is rejected at config validate.
Diagnostic byte budgets are in 「诊断容量边界」.

## 诊断容量边界

`[github_sync].diagnostic_budget_bytes` (default 4096) is the single
cap for receive-pack diagnostic output: side-band channel 2, side-band
channel 3, and SSH stderr. Values below 9 (the complete `truncated`
marker) are rejected at validate. The field is restart-required.
Side-band channel 2/3 payloads are counted and discarded after the
sink; they are not retained on the report buffer. Channel 3 and SSH
stderr take priority over channel 2 when the cap is full.

Collection stops at the cap. Overflow truncates on a UTF-8 character
boundary (no illegal fragment) and appends the `truncated` marker
(计划 AC-5「已截断」). Non-UTF-8 remote bytes are shown with U+FFFD
so ASCII around them survives (GS-15 / GS-25). Rendered diagnostic
bytes stay within the configured cap.
