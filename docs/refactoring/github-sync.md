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

## 同步语义冻结（Q1–Q5）

Spike GS-09. Freezes outbound **sync semantics** only. No production
code in this card. Pack error channels (Q6a), pack budget / `have`
hardening (Q6b), operator authorization (Q7), and atomic storage
primitives (Q8) stay on later spikes. `DEP-03` (full outbound draft)
was not found as a file in this tree; the schemes below are derived
from the plan-20260916 fact baseline and the cited code.

### Q1 — outbox trigger granularity

A trunk B3 push is one transaction that already mutates more than the
pushed path:

| Kind | What B3 writes | Evidence |
|---|---|---|
| Path upsert | `main@P` → `landed_at_p` | `apply_push_in_txn` (`mono_api_service.rs:3741`–`:3750`) |
| Ancestor roll-up | each ancestor whose tree hash **changed** gets a new synthesized commit; unchanged ancestors are skipped | `:3756`–`:3788` |
| Root CAS | `/` advances or does a net-zero same-value write | `:3791`–`:3836` |
| Descendant continuation | already-materialized descendant `main` refs | `push_queue_service.rs:2115`–`:2126`; `advance_descendant_refs` (`mono_storage.rs:631`) |
| Descendant delete | a descendant `main` ref is tombstoned and removed when its relative path is gone from the new tree | `DescendantResolve::Delete` → `tombstone_and_delete_main_ref_in_txn` (`mono_storage.rs:735`–`:737`, `:763`–`:769`; called from `push_queue_service.rs:2115`–`:2126`) |
| Failure / bypass | apply is rolled back; `BypassDetected` commits the Failed queue row and `notify_mono_write_queue`, not the apply | `push_queue_service.rs:2087`–`:2102`; authz precedent `notify.rs:164`–`:174` |

**Scheme.** After those writes, still inside the same B3 transaction
and only if the apply succeeded, enumerate a **change set** of
`(path, old_oid, new_oid, kind)`:

- `upsert` — `P` and every ancestor that received a new commit.
- `advance` — every descendant whose `main` ref moved.
- `delete` — every descendant that B3 resolved as
  `DescendantResolve::Delete` (the path `main` ref was
  tombstoned and removed in this transaction). The pushed path `P`
  itself is not deleted by B3 apply; a client receive-pack delete of
  `refs/heads/main` is refused (`monorepo.rs:913`–`:925`,
  `MEGA_BRANCH_NAME`). Tombstone repair / reaper deletes **outside**
  a successful B3 apply are not sync-outbox events (Q1 only fires
  after a successful apply in that same transaction).
- Skip net-zero rows (ancestor tree hash unchanged; root CAS
  same-value write).

Intersect the change set with `[github_sync].bindings` by **exact
`binding.path`** (ADR-GS-01: one path ↔ one `remote`; no implicit
prefix fan-out). Each hit inserts one sync-outbox row
`(binding_id, remote, kind, local_oid, queue_id)` in that same
transaction — the same shape as `insert_authz_outbox`
(`push_queue_storage.rs:1278`–`:1286`). `BypassDetected` and apply
failure must not insert sync rows. Root `/` is not a binding path.

`local_oid` is always the **new ref commit at that binding's exact
path**, not a single queue-wide id:

- `upsert` / `advance`: the new `main` commit written for that path.
  When the binding path is the pushed path `P` and `N=1`, that equals
  the client `new_id` (`docs/refactoring/trunk-push.md:429`;
  `api_tip_lander.rs:119`–`:120`). When `N>1` on `P`, it is `P`'s
  squash / synthesized commit (`push_queue.landed_commit_id` names
  **only** `P`). Ancestor and descendant rows use their own
  synthesized commits from `other_updates` /
  `advance_descendant_refs`.
- `delete`: there is no landed commit. `local_oid` is the all-zero
  object id of the repository hash kind (40 hex zeros for sha1; 64
  for sha256/blake3). `old_oid` on the change-set row is the path
  `main` commit that was just removed. Outbound receive-pack is
  `old=last_pushed` (must equal advertise), `new=zeros`,
  `refs/heads/main` — a Git delete of `main`. If GitHub `ng`s
  deleting the default branch, the binding parks with that reason
  (GS-15); an empty-tree replacement is a GS-10 alternative, not
  this freeze. First-create in Q5 is the inverse (`old=zeros`,
  `new=local_oid`).

### Q2 — pack closure and `have=[last_pushed]`

`incremental_pack` stops walking parents once a parent id is in
`have`, then excludes every object reachable from those `have`
commits' trees (`monorepo.rs:521`–`:566`). That is only a correct
thin pack if the GitHub repo still holds that closure.

**Scheme.** `have=[last_pushed]` is legal for a binding if and only if
all of:

1. `last_pushed` is either the `new` oid of the last receive-pack
   this binding recorded as success (GS-15/GS-25/GS-20/GS-26:
   `unpack ok` + `ok refs/heads/main` + `exit-status=0`) **or** an
   operator cursor-reset that Q3 already required to equal the
   current advertisement (so it is a proven remote tip, just not
   one this process created).
2. The current advertisement tip for `refs/heads/main` equals
   `last_pushed`.
3. `last_pushed` still exists in local object storage.
4. `binding.remote` has not changed since that success.

Advertisement tip equality is the only SSH-visible proof that GitHub
still has the object (ADR-GS-03: no REST; `DEFER-GS-02`: no fetch).
Any of: tip ≠ `last_pushed`, missing local object, empty repo (40
zero hex), or a `remote` string change **invalidates** the cursor.
Invalid → `have=[]` (full pack) and do not reuse the old cursor after
success until a new tip is recorded. Pack error-channel and byte
budget remain GS-18 / GS-27.

### Q3 — resume and expected tips

**Expected local tip** = the binding path's current `main` commit
(`local_oid` from Q1). **Expected remote tip** for a push attempt =
`last_pushed`, which must equal the advertisement.

**Divergence** = advertised `refs/heads/main` ≠ `last_pushed`.

mega2 cannot prove that a GitHub-side merge (human or AI) contains our
content: there is no merge-base, no 2-parent commit, and no inbound
fetch (ADR-GS-04; `push_chain.rs` rejects merge commits;
`DEFER-GS-02`). A third oid on GitHub is therefore **not** a resume
signal.

**Scheme.**

- Divergence **parks** the binding: no receive-pack.
- Divergence **clears** only when the advertisement equals
  `last_pushed` again (they reset to our last success) or an operator
  cursor-reset is applied **only after** the advertisement already
  equals `local_oid` (they made GitHub match us out of band).
  Who may invoke that reset is GS-21 Q7; this card only freezes the
  cursor predicate.
- Both sides' expected tips are the two oids above; validation is
  advertise == `last_pushed` immediately before send (Q4).
- GitHub-side merge that produces a new tip stays parked. Force-replace
  / inbound merge are not this card; they are a GS-10 alternative if a
  later plan accepts the history rewrite.

### Q4 — poll vs push race

A successful receive-pack updates GitHub **before** the local cursor
row. A poller that only compares advertise ≠ `last_pushed` would
treat our own in-flight tip as a remote edit.

**Scheme.** Persist an **intent** row before sending:

- `last_pushed` — last proven success (immutable on the success path
  until CAS).
- `in_flight` — `(new_oid, remote_old)` written **before** the
  command/pack; cleared only after the cursor CAS.

Poller classification:

| Advertised tip | Meaning |
|---|---|
| `last_pushed` | idle, not modified |
| `in_flight.new_oid` | self; complete the cursor CAS (crash after GitHub accept) |
| anything else | remote modified → Q3 park |

Order: write `in_flight` → advertise must still equal `remote_old`
(= `last_pushed`) → receive-pack → CAS `last_pushed := new`, clear
`in_flight`. A crash before GitHub accept leaves `in_flight` set and
advertise still `last_pushed`; retry is the same idempotent command
(Q5). A crash after accept leaves advertise == `in_flight.new_oid`;
the next worker finishes the CAS and does not park.

### Q5 — compensation identity and first create

**Scheme.** The idempotency key is
`(binding_id, remote, local_oid, remote_old)`. `remote_old` is the
advertised tip captured into `in_flight` **before** send and is
**not** overwritten by the success path (keep it on the completed
outbox / intent row). Retries with the same key replay the same
receive-pack command (`old`/`new`/`refs/heads/main`).

Compensation uses that stored `remote_old`, never the post-success
`last_pushed`:

- Failed **before** accept: retry the same key; do not invent a new
  `old`.
- Success then crash: Q4 self-complete; no compensate.
- First create: advertisement tip is 40 zero hex; `remote_old` is
  zeros; the command is a create. Compensation is **not** a remote
  delete (we do not delete GitHub `main`). If advertise later shows
  `local_oid`, treat as first-push success and CAS the cursor. If
  advertise is still zeros, retry the create. If advertise is some
  other oid, park (Q3).

### 三分支判定

| Field | Value |
|---|---|
| Branch | **go** |
| Timebox | 2026-09-20, single session after GS-26 `v0.38.18` (`9514a49`) |
| Basis | Q1–Q5 each have a scheme above, consistent with ADR-GS-01…04, the B3 change-set, and the receive-pack success contract |
| Not claimed | Automatic resume across a GitHub-side merge; inbound fetch; REST; any `src/` change |

### no-go 替代方向

N/A — this card is `go`. If a later review rejects Q3's park-only
resume, the alternative already named for GS-10 is an explicit
force-replace (or inbound merge) plan; do not silently treat a third
GitHub oid as fast-forward.

## pack 错误通道冻结（Q6a）

Spike GS-18. Freezes **in-stream pack errors, completion, and
cancel** for the outbound sync worker. Byte budget and `have`
negotiation stay on GS-27. No production code in this card.

Today `RepoHandler::incremental_pack` returns
`Result<ReceiverStream<Vec<u8>>, GitError>` (`pack/mod.rs:254`–`:258`).
The `Result` is only the setup failure. After the stream exists there
is no item-level error and no completion token. Both implementations
still `unwrap` storage lookups (`monorepo.rs` walk; `import_repo.rs:187`
–`:190`). Clone/fetch consume that stream at `smart.rs:283` and
`v2.rs:284`; `full_pack` (`monorepo.rs:248`) and default
`filtered_pack` (`pack/mod.rs:284`) are the other two call sites.

Changing that return type in place would force a behavior change onto
the live upload-pack path. This freeze **does not do that**.

### AC-1 — trait shape

**Scheme.** Keep `incremental_pack` byte-identical for clone/fetch.
Add a **parallel** method (name is DEP-01's; call it
`incremental_pack_reported` here) that is **not** on the live
upload-pack call sites:

```
async fn incremental_pack_reported(
    &self,
    want: Vec<String>,
    have: Vec<String>,
) -> Result<ReportedPackStream, GitError>
```

The method has a **default body** that returns
`GitError` “unsupported” so `ImportRepo` (and `Arc<dyn RepoHandler>`)
need not implement it. Only the monorepo handler used by github_sync
overrides it.

`ReportedPackStream` is a `Stream<Item = Result<Bytes, PackStreamError>>`:

| Item | Meaning |
|---|---|
| `Ok(chunk)` | pack window (same 16 KiB cap the receive-pack writer already uses) |
| `Err(e)` | terminal in-stream failure; stream ends |
| stream end (`None`) | **completion** — trailer was written; no extra success item |

`PackStreamError` is a closed enum: `Storage`, `Encode`, `Canceled`,
`Protocol`. Setup failures (bad want, handler missing) still return
from the `async fn` as `GitError`, same as today. The outbound worker
is the only required consumer in DEP-01. Migrating clone/fetch onto
this method is optional and out of this plan.

### AC-2 — consumer impact

| Site | Role | Impact of this freeze |
|---|---|---|
| `pack/mod.rs:254` | trait declaration | unchanged |
| `pack/monorepo.rs:497` | monorepo impl | unchanged; new method added beside it |
| `pack/import_repo.rs:175` | import-repo impl | unchanged; default “unsupported” body (sync is monorepo-path only, ADR-GS-01) |
| `protocol/smart.rs:283` | upload-pack v1 | **no change** |
| `protocol/v2.rs:284` | upload-pack v2 | **no change** |
| `pack/monorepo.rs:249` | `full_pack` → `incremental_pack` | **no change** |
| `pack/mod.rs:284` | `filtered_pack` default | **no change** |
| `pack/monorepo.rs:3281` | unit test | **no change** |

1 declaration / 2 implementations / 4 production call sites stay on
the existing `Vec<u8>` stream. That is how Q6a avoids regressing
clone/fetch. The new method's first production caller is the
github_sync worker (DEP-01).

### AC-3 — producer cancel (consumer drops the stream)

**Scheme.** Today's `incremental_pack` walks **inline** before
returning the stream (`monorepo.rs:497`–`:617`); only the encoder
is a spawned task. The reported method must **move the walk into a
task owned by the stream** (DEP-01 structural delta). The stream
owns:

- an `mpsc` (or equivalent) from that walker/encoder task, and
- a `CancellationToken` (or `Drop` flag) that the consumer drop
  cancels.

When the outbound worker drops the stream (timeout from GS-20,
park from GS-09 Q3, or process shutdown):

1. The token is cancelled.
2. The next `sender.send` fails or the walker sees the token and
   returns `Err(Canceled)`.
3. The producer task exits; it does not start a new tree walk or
   blob fold.

A late chunk already sitting in the channel may be lost; that is
acceptable because the worker has abandoned the receive-pack. The
producer must not `unwrap` a closed send (today's
`map_err` on send is the model, `pack/mod.rs:475`–`:481`).

### AC-4 — encoder-task cancel

**Scheme.** The pack encoder (the task that turns `Entry`s into
pack windows) is a child of the same token:

- On cancel: abort the encoder `JoinHandle` (or select on the token
  next to `entry_rx`). Drop the in-flight `Vec` from
  `try_fold` (`pack/mod.rs:464`–`:470`) without sending it.
- On `sender` closed: same path — treat as `Canceled`, not as
  `Encode`.
- Do not spawn an unbound encoder per blob; the existing
  `try_for_each_concurrent(16, …)` stays the concurrency cap
  (budget numbers are GS-27). Cancel must stop scheduling new
  concurrent folds.

Completion is the encoder finishing the pack trailer and closing
the chunk channel. The worker treats a clean `None` as success
only if it had already written that stream to receive-pack; a
cancel mid-trailer is `Canceled` and the GitHub attempt is not
recorded as success (GS-09 Q4/Q5).

### AC-5 — storage failure chain

**Scheme.** On the **reported** path only, every storage lookup
that today's impls `unwrap` (`get_commits_by_hashes`,
`get_commit_by_hash`, `get_trees_by_hashes`) maps `Err` to
`PackStreamError::Storage` and ends the stream. No `unwrap` /
`expect` / `panic` on that path (plan GC-11).

The live `incremental_pack` path is **not** rewritten by this
freeze (`DEFER-GS-07` still owns those unwraps). Clone/fetch keep
today's panic-or-disconnect behavior until a later plan migrates
them onto `incremental_pack_reported`.

A storage error that happens after some windows were already
sent is still terminal: the worker must not send those bytes as
a successful pack (GS-15 incomplete report). The first `Err`
item is the only diagnostic; later items are not produced.

### 三分支判定

| Field | Value |
|---|---|
| Branch | **go** |
| Timebox | 2026-09-20, same session as GS-09 `17c3905` |
| Basis | Additive reported stream; 4 live call sites untouched; cancel via token+drop; storage errors only on the new path |
| Not claimed | Rewriting clone/fetch; pack byte budget; `have` ACK semantics (GS-27) |

### no-go 替代方向

N/A — this card is `go`. The rejected alternative is changing
`incremental_pack`'s return type in place (would force smart.rs /
v2.rs to handle `Result` items and is a live-protocol rewrite).

## pack 资源预算与 have 语义冻结（Q6b）

Spike GS-27. Freezes **four byte budgets** on the reported pack
path (GS-18) and the **`have` distinction** that must not change
upload-pack. Live `incremental_pack` / clone / fetch stay as they
are. No production code in this card. Numbers below are DEP-01
defaults, not new `[github_sync]` keys in this plan.

Today the real allocation is the shared walker: each blob is
`try_fold`ed into one `Vec` at concurrency 16 (`pack/mod.rs:464`–
`:470`). The `mpsc` capacity is `pack.channel_message_size` slots
(default **1_000_000**, `src/config/model.rs:711`), not bytes.
Parent walk stops when a parent id is in `have`
(`monorepo.rs:521`–`:538`). Upload-pack ACKs only commits it
already has (`smart.rs:275`–`:276`).

### AC-1 — graph enumeration budget

**Scheme.** Bound the in-memory graph before any blob fold. Caps
depend on whether `have` is a proven tip or empty (GS-09 Q2):

| Set | What it holds | Incremental `have=[last_pushed]` | Full `have=[]` |
|---|---|---|---|
| `want_commits` / walk list | commits collected from `want` toward `have` (or to root) | `pack_max_walk_commits` = 4096 | `pack_max_walk_commits_full` = 65536 |
| `want_trees` | trees for those commits | same 4096 | same 65536 |
| `exist_objs` / `counted_obj` | hex ids already seen | `pack_max_object_ids` = 1_000_000 | same 1_000_000 |

`have=[]` is the first push and every cursor invalidation (Q2).
That walk is **full history to root**, not the delta between two
tips — the 4096 incremental cap must not be applied there, or a
repo with >4096 commits could never recover. Exceeding the
applicable cap is `PackStreamError::Encode` on the **reported**
stream and does not start encode. No silent truncate, no chunked
push in this plan: a binding that overflows the full cap fails
closed until the receiving plan raises the cap or splits history.
Derived id-set residency (not a second cap): 1_000_000 × ~64-byte
hex `String` keys ≈ 64–100 MiB for `exist_objs` / `counted_obj`.

Live `incremental_pack` is not rewritten (same as GS-18).

### AC-2 — concurrent blob accumulation budget

**Scheme.** The live default `traverse` (`pack/mod.rs:436`, fold at
`:464`) is shared by `incremental_pack`, `full_pack`, and import.
DEP-01 must **not** patch that default. The reported path gets a
fork (`traverse_budgeted` or a `BlobBudget` argument used only by
`incremental_pack_reported`).

On that fork: `try_for_each_concurrent(16, …)` is the **maximum**
concurrency; DEP-01 may lower it, not raise it. Each fold has a
**per-blob** cap `pack_max_blob_bytes` = 64 MiB. Crossing the cap
aborts that fold, cancels the other 15 (GS-18 AC-4), and emits
`PackStreamError::Encode`.

**Aggregate in-flight blob bytes** (AC-2 + AC-3 share this number):
`pack_max_inflight_blob_bytes` = `16 * pack_max_blob_bytes` (1 GiB).
That is the only blob-`Vec` residency allowed across concurrent
folds **and** the entry channel.

This budget is independent of Git LFS (LFS payloads are not pack
blobs). A blob that exceeds the per-blob cap is not chunked into
the pack.

### AC-3 — input / output queue budget

**Scheme.** Do **not** reuse `channel_message_size = 1_000_000` for
sync. The reported path uses small **slot** channels whose byte
residency is explicit. Entry slots **equal** fold concurrency so
the queue cannot hold a second 64-blob pile on top of AC-2:

| Channel | Slots | Payload cap per slot | Byte residency |
|---|---|---|---|
| walker → encoder (`entry_tx`) | 16 | one `Entry` ≤ `pack_max_blob_bytes` | counted inside `pack_max_inflight_blob_bytes` (1 GiB), not additive |
| encoder → worker (`stream_tx`) | 64 | `PACK_WRITE_WINDOW` (16 KiB, GS-24) | 64 × 16 KiB = 1 MiB |

The walker must not start a 17th fold while 16 entries are already
in flight (folding or queued). Backpressure is “send awaits or
cancel” (GS-18 AC-3). A full channel is not an error. The live pack
path may keep today’s slot count until a later plan migrates it.

`PACK_WRITE_WINDOW` is a **writer-side** re-chunk
(`send_pack.rs` `chunk.chunks(...)`), not a `PackEncoder` output
size. The 1 MiB stream residency is only true if DEP-01 inserts an
in-repo re-chunk between encoder and `stream_tx`. Do not patch
`git-internal`; wrap the encoder output. Without that wrap, 64
slots × arbitrary encoder chunks is the real residency.

### AC-4 — encode-process budget

**Scheme.** Encoder live residency is one in-flight `Entry` plus one
output window (16 KiB). It must not assemble the whole pack in
memory. `obj_num` from `traverse_for_count` is a count hint, not a
byte budget. `pack_max_encode_bytes` = 4 MiB is enforced **in-repo**
by counting bytes the wrapper hands to `stream_tx` (same wrap as
AC-3), not by patching `git-internal::PackEncoder`. Crossing the
cap is `PackStreamError::Encode`. Cancel drops the in-flight `Vec`
(GS-18 AC-4). Completion is still a clean stream `None` after the
trailer.

### AC-5 — two meanings of `have`

| Kind | Who sends it | Missing oid means | Action |
|---|---|---|---|
| **Negotiated `have`** | Git client on upload-pack | client advertised a commit we do not store | do **not** ACK (`smart.rs:275`); keep walking; **not an error** |
| **Must-traverse ancestor** | our walker, parent of a `want` we decided to include | local storage has no row (`get_commit_by_hash` `None` / `Err`) | **error** on the reported path (`PackStreamError::Storage`); invalidate is not “treat as have” |
| **Sync `have`** | github_sync worker | `last_pushed` missing locally, or advertise ≠ `last_pushed` | GS-09 Q2: invalidate cursor, `have=[]` (full pack) or park — **never** pass a GitHub-only oid as `have` |

A client-advertised unknown `have` is negotiation noise. A missing
**parent of a want we already accepted** is a broken graph.

### AC-6 — upload-pack negotiation unchanged

**Scheme.** AC-5’s “unknown have is not an error” is **exactly**
today’s upload-pack behavior and is frozen as a non-goal for this
plan: `smart.rs:275`–`:287` and `v2.rs` stay on
`incremental_pack(want, have)` with ACK-only-if-exists. DEP-01 must
not teach the live handler to fail closed on unknown client `have`.
The reported/sync path is a different caller and a different `have`
vector (Q2). Mixing those two `have` lists is a protocol bug.

### 三分支判定

| Field | Value |
|---|---|
| Branch | **go** |
| Timebox | 2026-09-20, after GS-18 `fabd01f` |
| Basis | Four caps on the reported path; live upload-pack `have` ACK rule left intact |
| Not claimed | Rewriting `PackConfig.channel_message_size` for clone/fetch; LFS; new config keys in this crate; patching `git-internal` |

### no-go 替代方向

N/A — this card is `go`. The rejected alternative is “any missing
`have` is an error” on `incremental_pack` (R2 / ADR-GS-06: that
breaks upload-pack negotiation).

### AC-8 — receiving-plan handoff

DEP-01 must implement, on `incremental_pack_reported` only:

1. Dual walk caps (AC-1): 4096 incremental / 65536 full (`have=[]`); typed overflow; no silent truncate.
2. Forked budgeted traverse (do not patch live `traverse`); concurrency ≤ 16; per-blob 64 MiB; aggregate in-flight blobs 1 GiB (AC-2).
3. Entry channel 16 slots (inside the 1 GiB blob cap) and stream channel 64 × 16 KiB windows (AC-3).
4. Encoder residency ≤ 4 MiB (AC-4).
5. Sync `have` is only `[last_pushed]` or `[]` per GS-09 Q2 (AC-5).
6. Live upload-pack continues to ignore unknown client `have` (AC-6).
7. Missing local parent of an accepted `want` is `PackStreamError::Storage` on the reported path. Live `incremental_pack` today panics (`monorepo.rs:527`–`:533` `unwrap().unwrap()`); that panic is **not** the reported-path contract (GS-18 AC-5).

## 操作授权冻结（Q7）

Spike GS-21. Freezes the **operator identity, token scope, and
`abort` / `force` predicates** for github_sync commands. This plan
still adds **no HTTP routes** (ADR-GS-02) and no CLI subcommand.
DEP-01 owns the surface. Atomic CAS of the cursor is GS-28 Q8; this
card only names who may request a write and when it is legal.

Trunk `/api/v1` has no `cedar_guard` (`http_server.rs:768`–`:771`;
review mounts it at `:792`). `push_auth=none` returns the stable
name `anonymous` (`api_write_auth.rs:34`). Agent-capture already
splits **identity** (`require_ingest_token` `:98`) from **scope**
(`token_covers_capture_repo` `:634`). Q7 copies that two-check
shape onto a **new** token family, not `git.push_tokens`.

### AC-1 — identity source

**Scheme.** Operator identity is a named token from
`[[github_sync.operator_tokens]]` (DEP-01 config; not added in this
crate). Presentation is the same Basic-password / Bearer header as
`authorize_trunk_api_write` (`api_write_auth.rs:58`–`:71`). The
requester name is `token.name`.

Not an identity source: session cookies, `UserStorage`, Cedar
principals, the bootstrap SSH key, `git.push_tokens`, or the
`anonymous` string. The SSH key authenticates **to GitHub**
(ADR-GS-03); it never authenticates an operator to mega2.

### AC-2 — credential scope

**Scheme.** Each operator token carries:

| Field | Meaning |
|---|---|
| `name` | identity (AC-1) |
| `token` | secret; compared like ingest tokens |
| `bindings` | binding `id` list, or `["*"]` for every configured binding |
| `actions` | subset of `status`, `abort`, `cursor_reset`, `force` |

A call is in scope only if **both** the target `binding.id` and the
verb are listed. `status` is read-only. `cursor_reset` is the Q3
“advertise already equals `local_oid`” write (no receive-pack).
`force` is the history-rewrite receive-pack. `*` in `bindings` does
not imply every action; omitted `actions` is empty (fail-closed).

### AC-3 — authorization model (`push_auth=none` included)

**Scheme.**

| Caller | `push_auth=none` | `push_auth=token` |
|---|---|---|
| monorepo git / trunk API write | `anonymous` (today) | `git.push_tokens` + path cover |
| github_sync operator command | **401** unless an operator token is presented | same: operator token required; a push token is not enough |

Missing or unknown operator secret → **401**. Known secret but
binding/action out of scope → **403** (the `api_write_auth`
path-cover shape, **not** agent-capture’s 404-on-scope). Operator
calls name a configured `binding.id`; hiding existence after a
valid secret is not required. `enabled=true` does **not** require
any operator token at boot (auto-park still works); commands are
simply unavailable until tokens exist. Review / Cedar stay out of
this plan (ADR-GS-02). DEP-01 must extend the GS-23 whitelist with
`github_sync.operator_tokens` when the table is added.

### AC-4 — `abort` conditions

Two distinct aborts; do not mix them.

**Policy abort (automatic).** On Q3 divergence (`advertise ≠
last_pushed` and advertise is not `in_flight.new_oid`), the worker
**parks** the binding and does not send receive-pack. No operator
token. This is the only automatic NFF response (ADR-GS-04: mega2
does not merge).

**Operator abort (command).** Allowed when:

1. Token has `abort` on that `binding.id` (AC-2).
2. The binding exists.
3. Binding state is one of the four frozen values below.

Frozen states (DEP-01 must not invent others):

| State | Meaning |
|---|---|
| `idle` | `last_pushed` equals advertise; no `in_flight` |
| `in_flight` | intent row set; receive-pack may be running or crash-pending (Q4) |
| `parked` | Q3 / operator abort; no automatic send |
| `disabled` | binding exists in config but sync is off |

`running` / `queued` are **not** states; they collapse into
`in_flight`. Operator abort of `idle` is a no-op 200.

It is **not** a GitHub update. It cancels the local pack (GS-18
token+drop), leaves `last_pushed` unchanged, and does not invent a
force-push. **Do not clear `in_flight` on operator abort** — Q4
self-complete (`advertise == in_flight.new_oid`) must still run if
GitHub already accepted. After an operator abort, clear `in_flight`
only on a later cursor CAS (success or Q4 self-complete). Policy
abort (nothing sent) may clear `in_flight` without a CAS. `parked`
and `disabled` abort are idempotent 200 (no cursor write).

### AC-5 — `abort` observable results

| Case | Binding | Cursor | Remote | Report |
|---|---|---|---|---|
| Policy abort (divergence) | `parked`, reason `remote_diverged` | `last_pushed` unchanged; `in_flight` cleared (nothing was sent) | no receive-pack | outbox / event `aborted` |
| Operator abort of `in_flight` | `parked`, reason `operator_abort` | `last_pushed` unchanged; **`in_flight` kept** | local pack cancelled; if advertise later equals `in_flight.new_oid`, Q4 self-complete still CAS-es | `aborted` + token `name` |
| Operator abort of already `parked` | stays `parked` | unchanged | none | 200 idempotent |
| Operator abort of `disabled` | stays `disabled` | unchanged | none | 200 idempotent |

After either abort the next automatic attempt stays parked until
advertise equals `last_pushed` again or a legal `cursor_reset` /
`force` runs (Q3).

### AC-6 — `force` conditions

**Never automatic.** A GitHub-side merge is not a resume signal
(Q3). Two operator writes exist:

**`cursor_reset`** (preferred; no history rewrite) when all of:

1. Token has `cursor_reset` on the binding.
2. Binding is `parked`.
3. Current advertise **equals** `local_oid` (Q3 already required
   this). Request repeats `expected_remote_tip` and
   `expected_local_oid`; mismatch → **409**, no write.
4. No receive-pack. Cursor CAS `last_pushed := local_oid`, unpark.
   The CAS primitive is GS-28.

**`force`** (history rewrite) when all of:

1. Token has `force` on the binding (`cursor_reset` is not enough).
2. Binding is `parked` for Q3 divergence.
3. Request repeats `expected_remote_tip` (= current advertise) and
   `expected_local_oid` (= path `main`). Mismatch → **409**.
4. Advertise is **not** `last_pushed` and **not** `local_oid`
   (those are idle / cursor_reset, not force).
5. Worker sends receive-pack
   `old=expected_remote_tip new=expected_local_oid refs/heads/main`.
   GitHub may `ng` a protected default branch (GS-15).

### AC-7 — `force` observable results

| Outcome | Binding | Cursor | Remote |
|---|---|---|---|
| `cursor_reset` 200 | `idle` | `last_pushed = local_oid`; `in_flight` empty | unchanged (already `local_oid`) |
| `force` unpack+`ok refs/heads/main`+exit 0 | `idle` | `last_pushed = local_oid` after CAS | GitHub `main` is `local_oid` (history of the previous tip discarded) |
| `force` `ng` / unpack fail / timeout | stays `parked` | `last_pushed` unchanged | GitHub tip unchanged |
| 401 / 403 / 409 | unchanged | unchanged | none |

A successful `force` is a **new** compensation key
`(binding_id, remote, local_oid, expected_remote_tip)` (Q5). It is
not a replay of the parked attempt.

### 三分支判定

| Field | Value |
|---|---|
| Branch | **go** |
| Timebox | 2026-09-20, after GS-27 `9da75a3` |
| Basis | Dedicated operator token family; `push_auth=none` never authorizes commands; automatic NFF is park-only; force is explicit + CAS |
| Not claimed | HTTP routes or CLI in this crate; Cedar on trunk; implementing the CAS primitive (GS-28); REST to GitHub |

### no-go 替代方向

N/A — this card is `go`. Rejected alternatives: reuse `anonymous` /
`git.push_tokens` for force; auto-force on a third GitHub oid;
mount `cedar_guard` on trunk as a hidden dep of this plan.
