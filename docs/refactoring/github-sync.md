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
generates a new key (public key differs). Public-key logging is GS-06.

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
bootstrap, start the rest, replace the GitHub deploy key with the new
public key.
