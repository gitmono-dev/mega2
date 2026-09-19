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
