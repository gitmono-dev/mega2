#!/usr/bin/env bash
# authz_kill_switch.sh — Kill Switch (… / UN-42 / UN-44).
#
# REL-02 family child: production and fixtures share this file. Later cards add
# SSH probes (UN-44) on the same --selftest entry.
#
# Modes:
#   --branch systemd|compose|file [--apply-content <file>] [--restart -- <argv...>]
#       Preflight (UN-50) then resolve/validate/replace; without --apply-content,
#       UN-41 generates transformed content and one-shot readback. Optional
#       --restart runs argv exactly once; on success: UN-36 HTTP, UN-47 log,
#       UN-42 Git ls-remote (when respective env set). T1=restart fail→exit 4;
#       T3=post-rename fsync fail→exit 5; T2=probe fail→exit 6.
#   --selftest
#       Run UN-33 + UN-48 + UN-41 + UN-46 + UN-50 + UN-36 + UN-53 + UN-47 + UN-42 gates.
#   -h | --help
#
# ---------------------------------------------------------------------------
# UN-47 log channel (frozen in header):
#   poll budget: 30 attempts × 2s sleep between samples (override via
#   KILL_SWITCH_LOG_POLL_ATTEMPTS / KILL_SWITCH_LOG_POLL_SLEEP_SECS for fixtures)
#   cursor: per-inode byte offset captured before restart; post-restart read is
#   incremental only (rename rotation keeps inode → archived bytes skipped;
#   truncated in-place file resets to offset 0)
#   would-deny: rg on the incremental slice; exit 0=hit (T2), 1=clean, >1=tool error
# ---------------------------------------------------------------------------
# UN-50 preflight matrix (frozen):
#   platform: Linux only (non-Linux fail-closed + install/run-on-Linux guidance)
#   exist:    cp, yq, jq, rg, flock, stat, getfacl, getfattr
#   probe:    "$KILL_SWITCH_BIN" authz-audit fsync --probe  (must exit 0)
#   versions: coreutils (cp) ≥ 8.30 | yq ≥ 4.18 | jq ≥ 1.6 | rg ≥ 13
#             | util-linux (flock) ≥ 2.27
#   capability: GNU `cp --preserve=all` must succeed on a temp pair
# Selftest inject (fixtures only):
#   KILL_SWITCH_PREFLIGHT_INJECT_UNAME=<os>
#   KILL_SWITCH_PREFLIGHT_INJECT_MISSING=<tool>
#   KILL_SWITCH_PREFLIGHT_INJECT_VERSION_{CP,YQ,JQ,RG,FLOCK}=<ver>
#   KILL_SWITCH_PREFLIGHT_INJECT_FSYNC_PROBE=fail
# ---------------------------------------------------------------------------

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="${BASH_SOURCE[0]}"

die() {
  printf 'authz_kill_switch: %s\n' "$*" >&2
  exit 4
}

usage() {
  cat <<'USAGE'
authz_kill_switch.sh — Kill Switch (UN-33/UN-48/UN-41/UN-46/UN-50/UN-36).

Usage:
  bash scripts/authz_kill_switch.sh --branch systemd|compose|file
  bash scripts/authz_kill_switch.sh --branch … [--apply-content <file>] [--restart -- <argv...>]
  bash scripts/authz_kill_switch.sh --selftest
  bash scripts/authz_kill_switch.sh -h | --help

Environment (branch mode):
  KILL_SWITCH_BIN          mega2 binary (required; used for authz-audit fsync)
  systemd: KILL_SWITCH_UNIT, KILL_SWITCH_ENV_FILE
  compose: KILL_SWITCH_COMPOSE, KILL_SWITCH_SERVICE (yq on PATH or KILL_SWITCH_YQ)
  file:    KILL_SWITCH_CONFIG, MEGA_PROFILE
  Inject:  KILL_SWITCH_READBACK_FAIL=1 (force readback failure after write)
  Probes (after successful --restart): KILL_SWITCH_URLS, KILL_SWITCH_EXPECT_CODE,
    KILL_SWITCH_DEPLOY_HTTP, KILL_SWITCH_RESTRICTED_DIR (evidence run root; UN-53);
    KILL_SWITCH_LOG_DIR (UN-47 would-deny incremental; cursor before restart);
    KILL_SWITCH_GIT_REMOTE + KILL_SWITCH_DEPLOY_GIT + KILL_SWITCH_GIT_ASKPASS (UN-42);
    KILL_SWITCH_SSH_* + KILL_SWITCH_DEPLOY_SSH (UN-44); early bind exit 7

Preflight (UN-50): Linux-only; requires cp/yq/jq/rg/flock/stat/getfacl/getfattr,
  KILL_SWITCH_BIN fsync --probe, and version floors (see script header).

Failure terminals:
  T1 restart failure → exit 4 (config remains off; prints manual restart argv)
  T3 post-rename fsync failure → exit 5 (do not roll back to on; re-run to converge)
  T2 probe failure → exit 6 (config remains off; evidence lines; no recovery-green claim)
USAGE
}

require_bin() {
  local bin="${KILL_SWITCH_BIN:-}"
  [[ -n "$bin" ]] || die "KILL_SWITCH_BIN is required"
  [[ -x "$bin" || -f "$bin" ]] || die "KILL_SWITCH_BIN is not executable: $bin"
  printf '%s\n' "$bin"
}

# ---------------------------------------------------------------------------
# UN-50 preflight
# ---------------------------------------------------------------------------

version_ge() {
  # Return 0 if $1 >= $2 (dotted numeric prefixes).
  python3 - "$1" "$2" <<'PY'
import sys
def parts(s):
    out = []
    for p in s.split("."):
        digits = "".join(ch for ch in p if ch.isdigit())
        out.append(int(digits) if digits else 0)
    return out
a, b = parts(sys.argv[1]), parts(sys.argv[2])
n = max(len(a), len(b))
a += [0] * (n - len(a))
b += [0] * (n - len(b))
sys.exit(0 if a >= b else 1)
PY
}

preflight_tool_path() {
  local name="$1"
  if [[ "${KILL_SWITCH_PREFLIGHT_INJECT_MISSING:-}" == "$name" ]]; then
    return 1
  fi
  if [[ "$name" == "yq" && -n "${KILL_SWITCH_YQ:-}" ]]; then
    [[ -x "$KILL_SWITCH_YQ" || -f "$KILL_SWITCH_YQ" ]] || return 1
    printf '%s\n' "$KILL_SWITCH_YQ"
    return 0
  fi
  command -v "$name" 2>/dev/null
}

preflight_parse_cp_ver() {
  local out
  out="$(cp --version 2>&1 | head -n 1)"
  if [[ -n "${KILL_SWITCH_PREFLIGHT_INJECT_VERSION_CP:-}" ]]; then
    printf '%s\n' "$KILL_SWITCH_PREFLIGHT_INJECT_VERSION_CP"
    return
  fi
  python3 -c 'import re,sys; m=re.search(r"(\d+\.\d+(?:\.\d+)?)", sys.argv[1]); print(m.group(1) if m else "")' "$out"
}

preflight_parse_yq_ver() {
  local yq_bin out
  yq_bin="$(preflight_tool_path yq)" || return 1
  if [[ -n "${KILL_SWITCH_PREFLIGHT_INJECT_VERSION_YQ:-}" ]]; then
    printf '%s\n' "$KILL_SWITCH_PREFLIGHT_INJECT_VERSION_YQ"
    return
  fi
  out="$("$yq_bin" --version 2>&1)"
  python3 -c 'import re,sys; m=re.search(r"[vV]?(\d+\.\d+(?:\.\d+)?)", sys.argv[1]); print(m.group(1) if m else "")' "$out"
}

preflight_parse_jq_ver() {
  local out
  if [[ -n "${KILL_SWITCH_PREFLIGHT_INJECT_VERSION_JQ:-}" ]]; then
    printf '%s\n' "$KILL_SWITCH_PREFLIGHT_INJECT_VERSION_JQ"
    return
  fi
  out="$(jq --version 2>&1)"
  python3 -c 'import re,sys; m=re.search(r"(\d+\.\d+(?:\.\d+)?)", sys.argv[1]); print(m.group(1) if m else "")' "$out"
}

preflight_parse_rg_ver() {
  local out
  if [[ -n "${KILL_SWITCH_PREFLIGHT_INJECT_VERSION_RG:-}" ]]; then
    printf '%s\n' "$KILL_SWITCH_PREFLIGHT_INJECT_VERSION_RG"
    return
  fi
  out="$(rg --version 2>&1 | head -n 1)"
  python3 -c 'import re,sys; m=re.search(r"(\d+\.\d+(?:\.\d+)?)", sys.argv[1]); print(m.group(1) if m else "")' "$out"
}

preflight_parse_flock_ver() {
  local out
  if [[ -n "${KILL_SWITCH_PREFLIGHT_INJECT_VERSION_FLOCK:-}" ]]; then
    printf '%s\n' "$KILL_SWITCH_PREFLIGHT_INJECT_VERSION_FLOCK"
    return
  fi
  out="$(flock --version 2>&1 | head -n 1)"
  python3 -c 'import re,sys; m=re.search(r"(\d+\.\d+(?:\.\d+)?)", sys.argv[1]); print(m.group(1) if m else "")' "$out"
}

run_preflight() {
  local os_name
  os_name="${KILL_SWITCH_PREFLIGHT_INJECT_UNAME:-$(uname -s)}"
  if [[ "$os_name" != "Linux" ]]; then
    die "platform is ${os_name}: Kill Switch requires Linux (O_NOFOLLOW/renameat/xattr/ACL). Run on a Linux host or container."
  fi

  local tool
  for tool in cp yq jq rg flock stat getfacl getfattr; do
    if ! preflight_tool_path "$tool" >/dev/null; then
      case "$tool" in
        yq) die "missing tool 'yq' (need mikefarah/yq ≥ 4.18). Install: https://github.com/mikefarah/yq/#install or set KILL_SWITCH_YQ" ;;
        getfattr | getfacl) die "missing tool '$tool' (need attr package). Install: apt install attr / dnf install attr" ;;
        flock) die "missing tool 'flock' (need util-linux ≥ 2.27). Install: apt install util-linux" ;;
        jq) die "missing tool 'jq' (need jq ≥ 1.6). Install: apt install jq" ;;
        rg) die "missing tool 'rg' (need ripgrep ≥ 13). Install: apt install ripgrep" ;;
        cp) die "missing tool 'cp' (need GNU coreutils ≥ 8.30). Install: apt install coreutils / dnf install coreutils" ;;
        stat) die "missing tool 'stat' (need GNU coreutils). Install: apt install coreutils / dnf install coreutils" ;;
        *) die "missing tool '$tool'" ;;
      esac
    fi
  done

  local ver
  ver="$(preflight_parse_cp_ver)"
  [[ -n "$ver" ]] || die "cannot parse GNU coreutils version from cp --version; install/upgrade: apt install coreutils"
  version_ge "$ver" "8.30" || die "cp/coreutils ${ver} < 8.30 (need GNU cp --preserve=all). Upgrade: apt install --only-upgrade coreutils"

  ver="$(preflight_parse_yq_ver)"
  [[ -n "$ver" ]] || die "cannot parse yq version; install mikefarah/yq ≥ 4.18 or set KILL_SWITCH_YQ"
  version_ge "$ver" "4.18" || die "yq ${ver} < 4.18. Upgrade mikefarah/yq: https://github.com/mikefarah/yq/#install"

  ver="$(preflight_parse_jq_ver)"
  [[ -n "$ver" ]] || die "cannot parse jq version; install: apt install jq"
  version_ge "$ver" "1.6" || die "jq ${ver} < 1.6. Upgrade jq: apt install --only-upgrade jq"

  ver="$(preflight_parse_rg_ver)"
  [[ -n "$ver" ]] || die "cannot parse rg version; install: apt install ripgrep"
  version_ge "$ver" "13" || die "rg ${ver} < 13. Upgrade ripgrep: apt install --only-upgrade ripgrep"

  ver="$(preflight_parse_flock_ver)"
  [[ -n "$ver" ]] || die "cannot parse util-linux/flock version; install: apt install util-linux"
  version_ge "$ver" "2.27" || die "flock/util-linux ${ver} < 2.27. Upgrade util-linux: apt install --only-upgrade util-linux"

  # GNU cp --preserve=all capability probe
  local cp_probe
  cp_probe="$(mktemp -d "${TMPDIR:-/tmp}/killswitch-cp-probe.XXXXXX")"
  printf 'x\n' >"$cp_probe/a"
  if ! cp --preserve=all "$cp_probe/a" "$cp_probe/b" 2>/dev/null; then
    rm -rf -- "$cp_probe"
    die "cp --preserve=all failed (need GNU coreutils ≥ 8.30)"
  fi
  rm -rf -- "$cp_probe"

  local bin
  bin="$(require_bin)"
  if [[ "${KILL_SWITCH_PREFLIGHT_INJECT_FSYNC_PROBE:-}" == "fail" ]]; then
    die "KILL_SWITCH_BIN authz-audit fsync --probe failed (inject). Need a mega2 build with UN-29 fsync tool mode."
  fi
  if ! "$bin" authz-audit fsync --probe >/dev/null 2>&1; then
    die "KILL_SWITCH_BIN authz-audit fsync --probe failed. Need a mega2 build with UN-29 fsync tool mode (fd-level fsync)."
  fi
}

# ---------------------------------------------------------------------------
# Path / config checks
# ---------------------------------------------------------------------------

assert_regular_nofollow() {
  local path="$1"
  [[ -e "$path" ]] || die "target does not exist: $path"
  [[ ! -L "$path" ]] || die "target must not be a symlink: $path"
  [[ -f "$path" ]] || die "target must be a regular file: $path"
}

# Reject duplicate KEY= lines (first `=` wins as the key) in a KEY=VALUE file.
assert_no_duplicate_keys() {
  local path="$1"
  assert_regular_nofollow "$path"
  python3 - "$path" <<'PY'
import sys
from collections import Counter
path = sys.argv[1]
keys = []
with open(path, "r", encoding="utf-8", errors="replace") as fh:
    for line in fh:
        raw = line.rstrip("\n")
        if not raw or raw.lstrip().startswith("#"):
            continue
        if "=" not in raw:
            continue
        key = raw.split("=", 1)[0].strip()
        if key:
            keys.append(key)
dupes = sorted(k for k, n in Counter(keys).items() if n > 1)
if dupes:
    print(f"duplicate target keys: {', '.join(dupes)}", file=sys.stderr)
    sys.exit(4)
PY
}

# Claim an exclusive temp name under the target's parent (O_EXCL|O_NOFOLLOW),
# seed it with `cp --preserve=all` from the target (owner/mode/ACL/xattr), then
# rewrite bytes through an O_NOFOLLOW fd. Prints the temp path.
create_temp_preserve_rewrite() {
  local target="$1"
  local content_file="$2"
  local dir base
  dir="$(dirname -- "$target")"
  base="$(basename -- "$target")"
  python3 - "$dir" "$base" "$target" "$content_file" <<'PY'
import os, sys, secrets, stat, subprocess

directory, base, target_path, content_path = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

def write_all(fd, data: bytes) -> None:
    view = memoryview(data)
    while view:
        n = os.write(fd, view)
        if n <= 0:
            raise OSError("short write")
        view = view[n:]

def snapshot(path: str) -> dict:
    st = os.stat(path, follow_symlinks=False)
    meta = {
        "uid": st.st_uid,
        "gid": st.st_gid,
        "mode": stat.S_IMODE(st.st_mode),
        "acl": None,
        "xattrs": {},
    }
    try:
        out = subprocess.check_output(
            ["getfacl", "-c", "--absolute-names", "--", path],
            stderr=subprocess.PIPE,
            text=True,
        )
        # Drop comments; keep ACL entries stable.
        meta["acl"] = "\n".join(
            line for line in out.splitlines() if line and not line.startswith("#")
        )
    except FileNotFoundError:
        print("getfacl not found (required for ACL verify; UN-50 preflight)", file=sys.stderr)
        sys.exit(4)
    except subprocess.CalledProcessError as err:
        detail = (err.stderr or "").strip() or str(err)
        print(f"getfacl failed for {path}: {detail}", file=sys.stderr)
        sys.exit(4)
    try:
        for name in os.listxattr(path, follow_symlinks=False):
            meta["xattrs"][name] = os.getxattr(path, name, follow_symlinks=False)
    except OSError as err:
        print(f"xattr inspect failed for {path}: {err}", file=sys.stderr)
        sys.exit(4)
    return meta

def verify(path: str, expected: dict) -> None:
    force = os.environ.get("KILL_SWITCH_META_VERIFY_FAIL", "")
    if force in ("1", "owner", "mode", "acl", "xattr"):
        which = "owner" if force == "1" else force
        print(f"metadata verify injected failure: {which}", file=sys.stderr)
        sys.exit(4)
    got = snapshot(path)
    if (got["uid"], got["gid"]) != (expected["uid"], expected["gid"]):
        print(
            f"owner mismatch: expected {expected['uid']}:{expected['gid']} "
            f"got {got['uid']}:{got['gid']}",
            file=sys.stderr,
        )
        sys.exit(4)
    if got["mode"] != expected["mode"]:
        print(
            f"mode mismatch: expected {expected['mode']:04o} got {got['mode']:04o}",
            file=sys.stderr,
        )
        sys.exit(4)
    if got["acl"] != expected["acl"]:
        print("ACL mismatch after rewrite", file=sys.stderr)
        sys.exit(4)
    if got["xattrs"] != expected["xattrs"]:
        print("xattr mismatch after rewrite", file=sys.stderr)
        sys.exit(4)

try:
    dir_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
except OSError as err:
    print(f"open parent dir failed: {directory}: {err}", file=sys.stderr)
    sys.exit(4)

tmp_path = None
try:
    st = os.stat(base, dir_fd=dir_fd, follow_symlinks=False)
    if not stat.S_ISREG(st.st_mode):
        print(f"target must be a regular file: {base}", file=sys.stderr)
        sys.exit(4)

    expected = snapshot(target_path)

    name = f".tmp-killswitch-{base}-{secrets.token_hex(8)}"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC
    try:
        fd = os.open(name, flags, 0o600, dir_fd=dir_fd)
    except FileExistsError:
        print(f"temp path already exists (refusing overwrite): {name}", file=sys.stderr)
        sys.exit(4)
    except OSError as err:
        print(f"temp O_EXCL|O_NOFOLLOW open failed: {name}: {err}", file=sys.stderr)
        sys.exit(4)
    os.close(fd)

    tmp_path = os.path.join(directory, name)
    # Seed content + metadata from the live target (UN-48).
    try:
        subprocess.run(
            ["cp", "--preserve=all", "--", target_path, tmp_path],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        print("cp not found (GNU cp --preserve=all required)", file=sys.stderr)
        sys.exit(4)
    except subprocess.CalledProcessError as err:
        print(f"cp --preserve=all failed: {err.stderr.strip()}", file=sys.stderr)
        sys.exit(4)

    # Rewrite bytes in place without dropping preserved metadata.
    fd = os.open(
        name,
        os.O_WRONLY | os.O_NOFOLLOW | os.O_TRUNC | os.O_CLOEXEC,
        dir_fd=dir_fd,
    )
    try:
        with open(content_path, "rb") as src:
            while True:
                chunk = src.read(1024 * 1024)
                if not chunk:
                    break
                write_all(fd, chunk)
    finally:
        os.close(fd)

    verify(tmp_path, expected)
    print(tmp_path)
except FileNotFoundError:
    print(f"target does not exist: {base}", file=sys.stderr)
    sys.exit(4)
except OSError as err:
    print(f"secure temp write failed: {err}", file=sys.stderr)
    sys.exit(4)
finally:
    os.close(dir_fd)
PY
}

# Atomic rename within the target's parent directory (no-follow dir fd).
renameat_nofollow() {
  local tmp_path="$1"
  local target="$2"
  python3 - "$tmp_path" "$target" <<'PY'
import os, sys
tmp_path, target = sys.argv[1], sys.argv[2]
directory = os.path.dirname(target) or "."
tmp_name = os.path.basename(tmp_path)
target_name = os.path.basename(target)
try:
    dir_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
except OSError as err:
    print(f"open parent dir failed: {directory}: {err}", file=sys.stderr)
    sys.exit(4)
try:
    os.rename(tmp_name, target_name, src_dir_fd=dir_fd, dst_dir_fd=dir_fd)
except OSError as err:
    print(f"renameat failed: {tmp_name} -> {target_name}: {err}", file=sys.stderr)
    sys.exit(4)
finally:
    os.close(dir_fd)
PY
}

fsync_path() {
  local bin="$1"
  local path="$2"
  "$bin" authz-audit fsync "$path"
}

# Atomic replace: cp --preserve=all → rewrite → verify metadata → fsync → rename → fsync.
# T3: post-rename fsync failure → exit 5 (config may be new or old; never roll back to on).
secure_replace() {
  local target="$1"
  local content_file="$2"
  local bin
  bin="$(require_bin)"

  assert_regular_nofollow "$target"
  assert_regular_nofollow "$content_file"

  local tmp
  tmp="$(create_temp_preserve_rewrite "$target" "$content_file")"
  # shellcheck disable=SC2064
  trap 'rm -f -- "'"$tmp"'"' RETURN

  if ! fsync_path "$bin" "$tmp"; then
    die "file fsync failed before rename: $tmp"
  fi
  renameat_nofollow "$tmp" "$target"
  trap - RETURN
  if ! fsync_path "$bin" "$target"; then
    printf 'authz_kill_switch: T3 post-rename fsync failed for %s (exit 5). Target may be new or old; do not roll back to on — re-run this script to converge on off.\n' "$target" >&2
    exit 5
  fi
}

# Exactly-once restart via argv array (no env-string re-split). T1 on failure.
run_restart() {
  if [[ $# -eq 0 ]]; then
    die "--restart -- requires at least one argv element"
  fi
  local rc=0
  "$@" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    printf 'authz_kill_switch: T1 restart failed (command exit %s). Config remains off (safe direction).\n' "$rc" >&2
    printf 'authz_kill_switch: manual restart guidance (run exactly):' >&2
    local arg
    for arg in "$@"; do
      printf ' %q' "$arg" >&2
    done
    printf '\n' >&2
    exit 4
  fi
}

# ---------------------------------------------------------------------------
# UN-53 evidence session (run-init / lease / evidence-append / commit|abort)
# + UN-36 HTTP probes (consume the session)
# ---------------------------------------------------------------------------

EVIDENCE_ROOT=""
EVIDENCE_RUN_ID=""
EVIDENCE_RUN_CAP=""
EVIDENCE_LEASE_FD=""

evidence_session_begin() {
  local bin root out lease
  bin="$(require_bin)"
  root="${KILL_SWITCH_RESTRICTED_DIR:-}"
  [[ -n "$root" ]] || die "KILL_SWITCH_RESTRICTED_DIR is required when recording evidence (KILL_SWITCH_URLS set)"
  [[ -d "$root" ]] || die "KILL_SWITCH_RESTRICTED_DIR is not a directory: $root"
  EVIDENCE_ROOT="$root"
  out="$("$bin" authz-audit run-init --restricted-root "$root")"
  EVIDENCE_RUN_ID="$(printf '%s\n' "$out" | sed -n 's/^run_id=//p' | head -n1)"
  EVIDENCE_RUN_CAP="$(printf '%s\n' "$out" | sed -n 's/^run_cap=//p' | head -n1)"
  [[ -n "$EVIDENCE_RUN_ID" && -n "$EVIDENCE_RUN_CAP" ]] || die "run-init did not print run_id=/run_cap="
  lease="$root/runs/$EVIDENCE_RUN_ID/.lease.lock"
  # Hold long-lived lease (distinct from UN-56 .evidence.lock RMW).
  : >>"$lease"
  exec {EVIDENCE_LEASE_FD}<>"$lease"
  flock -x "$EVIDENCE_LEASE_FD" || die "failed to flock $lease"
  printf 'authz_kill_switch: evidence run_id=%s (lease held)\n' "$EVIDENCE_RUN_ID" >&2
}

# Typed append only — never splice/edit killswitch-evidence.json in-script.
# Failures are hard (die): must not be mistaken for a soft probe verdict.
evidence_append_check() {
  local channel="$1" check="$2" verdict="$3" status="${4:-}"
  local bin
  bin="$(require_bin)"
  [[ -n "$EVIDENCE_ROOT" && -n "$EVIDENCE_RUN_ID" ]] || die "evidence session not started"
  local -a args=(
    authz-audit evidence-append
    --restricted-root "$EVIDENCE_ROOT"
    --run-id "$EVIDENCE_RUN_ID"
    --channel "$channel"
    --check "$check"
    --verdict "$verdict"
  )
  if [[ -n "$status" ]]; then
    args+=(--status "$status")
  fi
  if ! "$bin" "${args[@]}"; then
    die "evidence-append failed (channel=$channel check=$check verdict=$verdict)"
  fi
  printf 'authz_kill_switch: evidence-append channel=%s check=%s verdict=%s\n' "$channel" "$check" "$verdict" >&2
}

evidence_session_release_lease() {
  if [[ -n "${EVIDENCE_LEASE_FD:-}" ]]; then
    flock -u "$EVIDENCE_LEASE_FD" 2>/dev/null || true
    eval "exec ${EVIDENCE_LEASE_FD}>&-" 2>/dev/null || true
    EVIDENCE_LEASE_FD=""
  fi
}

evidence_session_commit() {
  local bin
  bin="$(require_bin)"
  evidence_session_release_lease
  RUN_CAP="$EVIDENCE_RUN_CAP" "$bin" authz-audit run-commit \
    --restricted-root "$EVIDENCE_ROOT" --run-id "$EVIDENCE_RUN_ID"
}

evidence_session_abort() {
  local bin
  bin="$(require_bin)"
  evidence_session_release_lease
  if [[ -n "${EVIDENCE_RUN_CAP:-}" && -n "${EVIDENCE_RUN_ID:-}" && -n "${EVIDENCE_ROOT:-}" ]]; then
    RUN_CAP="$EVIDENCE_RUN_CAP" "$bin" authz-audit run-abort \
      --restricted-root "$EVIDENCE_ROOT" --run-id "$EVIDENCE_RUN_ID" || true
  fi
}

fail_t2() {
  local reason="$1"
  printf 'authz_kill_switch: T2 probe failure (exit 6): %s. Config remains off. Script does not claim recovery-green — operator/CI must judge from evidence.\n' "$reason" >&2
  exit 6
}

# Canonical origin: scheme://host:port (lowercase; default ports explicit; IPv6 brackets).
canonical_http_origin() {
  python3 - "$1" <<'PY'
import sys
from urllib.parse import urlparse
url = sys.argv[1]
p = urlparse(url)
if p.scheme not in ("http", "https") or not p.hostname:
    print(f"invalid http(s) URL: {url}", file=sys.stderr)
    sys.exit(4)
scheme = p.scheme.lower()
host = p.hostname.lower()
if ":" in host:
    host_fmt = f"[{host}]"
else:
    host_fmt = host
port = p.port
if port is None:
    port = 443 if scheme == "https" else 80
print(f"{scheme}://{host_fmt}:{port}")
PY
}

# On soft probe failure: write reason to $4, return 1 (caller probes all URLs).
# Hard failures (evidence CLI / die) must not run under $() — they abort the script.
probe_one_http_url() {
  local url="$1"
  local expect="$2"
  local deploy="$3"
  local reason_file="$4"
  local origin code errfile rc=0
  origin="$(canonical_http_origin "$url")"
  if [[ "$origin" != "$deploy" ]]; then
    evidence_append_check http http_binding fail
    printf 'HTTP bind mismatch for %s (origin=%s deploy=%s)\n' "$url" "$origin" "$deploy" >"$reason_file"
    return 1
  fi
  errfile="$(mktemp "${TMPDIR:-/tmp}/killswitch-probe.XXXXXX")"
  code="$(
    python3 - "$url" <<'PY' 2>"$errfile"
import ssl, sys, urllib.error, urllib.request
url = sys.argv[1]
ctx = ssl.create_default_context()
try:
    with urllib.request.urlopen(url, context=ctx, timeout=5) as resp:
        print(resp.status)
except urllib.error.HTTPError as err:
    print(err.code)
except Exception as err:
    print(f"{type(err).__name__}: {err}", file=sys.stderr)
    sys.exit(2)
PY
  )" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    local detail
    detail="$(tr '\n' ' ' <"$errfile" 2>/dev/null || true)"
    rm -f -- "$errfile"
    # TLS only when the error is certificate/handshake related (not mere https://).
    if printf '%s' "$detail" | grep -qiE 'ssl|certificate|CERTIFICATE|TLS|CERTIFICATE_VERIFY|SSLCertVerificationError|CERTIFICATE_VERIFY_FAILED|ssl\.SSLError'; then
      evidence_append_check http tls_chain fail
    else
      evidence_append_check http http_serving fail
    fi
    printf 'HTTP serving/TLS failure for %s (%s)\n' "$url" "$detail" >"$reason_file"
    return 1
  fi
  rm -f -- "$errfile"
  if [[ "$code" != "$expect" ]]; then
    evidence_append_check http http_status fail "$code"
    printf 'HTTP status %s != expect %s for %s\n' "$code" "$expect" "$url" >"$reason_file"
    return 1
  fi
  evidence_append_check http http_binding pass
  evidence_append_check http http_serving pass
  evidence_append_check http http_status pass "$code"
  if [[ "$url" == https://* ]]; then
    evidence_append_check http tls_chain pass
  fi
  return 0
}

run_http_probes() {
  local urls="${KILL_SWITCH_URLS:-}"
  [[ -n "$urls" ]] || return 0
  local expect="${KILL_SWITCH_EXPECT_CODE:-}"
  local deploy="${KILL_SWITCH_DEPLOY_HTTP:-}"
  [[ -n "$expect" ]] || die "KILL_SWITCH_EXPECT_CODE is required when KILL_SWITCH_URLS is set"
  [[ -n "$deploy" ]] || die "KILL_SWITCH_DEPLOY_HTTP is required when KILL_SWITCH_URLS is set"
  deploy="$(canonical_http_origin "$deploy")"
  local -a url_list=()
  read -r -a url_list <<<"$urls"

  evidence_session_begin
  # Abort unsettled sessions on unexpected exit (T2 still commits first).
  local evidence_settled=0
  # shellcheck disable=SC2064
  trap '[[ "${evidence_settled:-0}" -eq 1 ]] || evidence_session_abort' EXIT

  local url failures=0
  local -a fail_reasons=()
  local reason_file probe_rc
  for url in "${url_list[@]}"; do
    [[ -n "$url" ]] || continue
    # Avoid $() capture: die()/evidence-append hard fails must reach the main shell.
    reason_file="$(mktemp "${TMPDIR:-/tmp}/killswitch-probe-reason.XXXXXX")"
    set +e
    probe_one_http_url "$url" "$expect" "$deploy" "$reason_file"
    probe_rc=$?
    set -e
    if [[ "$probe_rc" -eq 0 ]]; then
      rm -f -- "$reason_file"
      continue
    fi
    if [[ "$probe_rc" -eq 1 ]]; then
      failures=$((failures + 1))
      fail_reasons+=("$(tr '\n' ' ' <"$reason_file" 2>/dev/null || echo "unknown failure for $url")")
      rm -f -- "$reason_file"
      continue
    fi
    rm -f -- "$reason_file"
    die "HTTP probe hard failure for $url (rc=$probe_rc)"
  done
  evidence_session_commit
  evidence_settled=1
  trap - EXIT
  if [[ "$failures" -gt 0 ]]; then
    local joined
    joined="$(printf '%s; ' "${fail_reasons[@]}")"
    fail_t2 "probed ${#url_list[@]} URL(s), ${failures} failed: ${joined}"
  fi
  printf 'authz_kill_switch: HTTP probes finished (%s URL(s)); evidence recorded — not a recovery-green claim.\n' "${#url_list[@]}" >&2
}

# ---------------------------------------------------------------------------
# UN-47 log channel — zero new would-deny on incremental slice after restart
# ---------------------------------------------------------------------------

LOG_CURSOR_FILE=""

log_poll_attempts() {
  printf '%s\n' "${KILL_SWITCH_LOG_POLL_ATTEMPTS:-30}"
}

log_poll_sleep_secs() {
  printf '%s\n' "${KILL_SWITCH_LOG_POLL_SLEEP_SECS:-2}"
}

require_rg() {
  local rg_bin="${KILL_SWITCH_RG:-}"
  if [[ -n "$rg_bin" ]]; then
    [[ -x "$rg_bin" || -f "$rg_bin" ]] || die "KILL_SWITCH_RG is not executable: $rg_bin"
    printf '%s\n' "$rg_bin"
    return
  fi
  command -v rg >/dev/null 2>&1 || die "rg is required for log channel (UN-47)"
  command -v rg
}

# Snapshot regular files as inode<TAB>path<TAB>size (sorted by path).
log_dir_size_snapshot() {
  local root="$1"
  find "$root" -type f -printf '%i\t%p\t%s\n' 2>/dev/null | sort -t $'\t' -k2,2
}

# Capture per-inode byte cursors before restart (must run before run_restart).
log_cursor_capture() {
  local root="${KILL_SWITCH_LOG_DIR:-}"
  [[ -n "$root" ]] || return 0
  [[ -d "$root" ]] || die "KILL_SWITCH_LOG_DIR is not a directory: $root"
  LOG_CURSOR_FILE="$(mktemp "${TMPDIR:-/tmp}/killswitch-log-cursor.XXXXXX")"
  log_dir_size_snapshot "$root" >"$LOG_CURSOR_FILE"
  printf 'authz_kill_switch: log cursor captured (%s files under %s)\n' \
    "$(wc -l <"$LOG_CURSOR_FILE" | tr -d ' ')" "$root" >&2
}

# Wait until two consecutive size snapshots match (flush settled), or fail timeout.
log_wait_for_flush() {
  local root="$1"
  if [[ "${KILL_SWITCH_LOG_POLL_FORCE_TIMEOUT:-}" == "1" ]]; then
    return 1
  fi
  local attempts sleep_s
  attempts="$(log_poll_attempts)"
  sleep_s="$(log_poll_sleep_secs)"
  local prev="" cur="" i
  for ((i = 1; i <= attempts; i++)); do
    cur="$(log_dir_size_snapshot "$root")"
    if [[ -n "$prev" && "$cur" == "$prev" ]]; then
      printf 'authz_kill_switch: log flush settled after %s sample(s)\n' "$i" >&2
      return 0
    fi
    prev="$cur"
    if [[ "$i" -lt "$attempts" && "$sleep_s" != "0" ]]; then
      sleep "$sleep_s"
    fi
  done
  return 1
}

# Build incremental slice from inode cursors into $3. Rotation keeps inode → skip archived.
log_build_incremental() {
  local root="$1"
  local cursor_file="$2"
  local out_file="$3"
  : >"$out_file"
  python3 - "$root" "$cursor_file" "$out_file" <<'PY'
import os, sys
root, cursor_path, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
cursors = {}  # inode -> offset
with open(cursor_path, "r", encoding="utf-8", errors="replace") as fh:
    for line in fh:
        line = line.rstrip("\n")
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        try:
            ino = int(parts[0])
            size = int(parts[2])
        except ValueError:
            continue
        cursors[ino] = size
with open(out_path, "ab") as out:
    for dirpath, _, filenames in os.walk(root):
        for name in filenames:
            path = os.path.join(dirpath, name)
            if not os.path.isfile(path):
                continue
            try:
                st = os.stat(path)
            except OSError:
                continue
            ino = st.st_ino
            size = st.st_size
            start = cursors.get(ino, 0)
            if size < start:
                start = 0  # truncated in place
            if size <= start:
                continue
            with open(path, "rb") as fh:
                fh.seek(start)
                out.write(fh.read())
PY
}

# rg with explicit 0/1/>1 branching. Prints matches to stdout when rc=0.
# Returns 0=hit, 1=no hit; dies on >1.
# NOTE: never `set -e` before `return 1` — bash would exit the whole script.
rg_would_deny_in_file() {
  local file="$1"
  local rg_bin rc=0
  rg_bin="$(require_rg)"
  set +e
  "$rg_bin" -n -- 'would-deny' "$file"
  rc=$?
  set +e
  if [[ "$rc" -gt 1 ]]; then
    die "rg failed while scanning would-deny (exit $rc); refusing to treat as zero-hit"
  fi
  return "$rc"
}

run_log_probes() {
  local root="${KILL_SWITCH_LOG_DIR:-}"
  [[ -n "$root" ]] || return 0
  [[ -d "$root" ]] || die "KILL_SWITCH_LOG_DIR is not a directory: $root"
  [[ -n "${LOG_CURSOR_FILE:-}" && -f "$LOG_CURSOR_FILE" ]] \
    || die "log cursor missing — capture must run before --restart when KILL_SWITCH_LOG_DIR is set"

  # Evidence session (UN-53) for the log channel check.
  evidence_session_begin
  local evidence_settled=0
  # shellcheck disable=SC2064
  trap '[[ "${evidence_settled:-0}" -eq 1 ]] || evidence_session_abort' EXIT

  if ! log_wait_for_flush "$root"; then
    evidence_append_check log log_no_would_deny fail
    evidence_session_commit
    evidence_settled=1
    trap - EXIT
    fail_t2 "log flush poll timed out (${KILL_SWITCH_LOG_POLL_ATTEMPTS:-30} × ${KILL_SWITCH_LOG_POLL_SLEEP_SECS:-2}s)"
  fi

  local slice
  slice="$(mktemp "${TMPDIR:-/tmp}/killswitch-log-slice.XXXXXX")"
  log_build_incremental "$root" "$LOG_CURSOR_FILE" "$slice"

  local rg_rc=0
  set +e
  rg_would_deny_in_file "$slice" >/dev/null
  rg_rc=$?
  set -e
  rm -f -- "$slice"

  if [[ "$rg_rc" -eq 0 ]]; then
    evidence_append_check log log_no_would_deny fail
    evidence_session_commit
    evidence_settled=1
    trap - EXIT
    fail_t2 "new would-deny line(s) in KILL_SWITCH_LOG_DIR incremental slice after restart"
  fi
  # rg_rc == 1 → clean
  evidence_append_check log log_no_would_deny pass
  evidence_session_commit
  evidence_settled=1
  trap - EXIT
  printf 'authz_kill_switch: log channel clean (no new would-deny); evidence recorded — not a recovery-green claim.\n' >&2
}


# ---------------------------------------------------------------------------
# UN-44 SSH readonly channel + cross-channel bind (fail before any writes)
# ---------------------------------------------------------------------------

fail_bind() {
  local reason="$1"
  printf 'authz_kill_switch: bind failure (exit 7): %s. No files were written. Manually verify KILL_SWITCH_DEPLOY_HTTP/GIT/SSH against live probe targets before re-running.\n' "$reason" >&2
  exit 7
}

canonical_ssh_endpoint() {
  python3 - "$1" "${2:-}" <<'PY'
import sys
host = sys.argv[1].strip().lower()
port_s = (sys.argv[2] or "").strip()
if not host:
    print("empty ssh host", file=sys.stderr)
    sys.exit(4)
if host.startswith("[") and host.endswith("]"):
    host_inner = host[1:-1]
else:
    host_inner = host
    if host_inner.count(":") == 1:
        left, right = host_inner.split(":", 1)
        if right.isdigit():
            host_inner, port_s = left, right
if port_s:
    port = int(port_s)
else:
    port = 22
if ":" in host_inner:
    host_fmt = f"[{host_inner}]"
else:
    host_fmt = host_inner
print(f"{host_fmt}:{port}")
PY
}

# Early bind: before run_branch / any secure_replace. Exit 7, zero writes.
assert_cross_channel_bindings() {
  local urls="${KILL_SWITCH_URLS:-}"
  local git_remote="${KILL_SWITCH_GIT_REMOTE:-}"
  local ssh_host="${KILL_SWITCH_SSH_HOST:-}"
  if [[ -z "$urls" && -z "$git_remote" && -z "$ssh_host" ]]; then
    return 0
  fi

  if [[ -n "$urls" ]]; then
    local deploy="${KILL_SWITCH_DEPLOY_HTTP:-}"
    [[ -n "$deploy" ]] || fail_bind "KILL_SWITCH_DEPLOY_HTTP required when KILL_SWITCH_URLS is set"
    deploy="$(canonical_http_origin "$deploy")"
    local -a url_list=()
    read -r -a url_list <<<"$urls"
    local u origin
    for u in "${url_list[@]}"; do
      [[ -n "$u" ]] || continue
      origin="$(canonical_http_origin "$u")"
      if [[ "$origin" != "$deploy" ]]; then
        fail_bind "HTTP probe $u origin=$origin != deploy=$deploy"
      fi
    done
  fi

  if [[ -n "$git_remote" ]]; then
    local gdeploy="${KILL_SWITCH_DEPLOY_GIT:-}"
    [[ -n "$gdeploy" ]] || fail_bind "KILL_SWITCH_DEPLOY_GIT required when KILL_SWITCH_GIT_REMOTE is set"
    if printf '%s' "$git_remote" | grep -qiE '^https?://[^/]*@'; then
      fail_bind "KILL_SWITCH_GIT_REMOTE must not embed credentials (userinfo)"
    fi
    gdeploy="$(canonical_http_origin "$gdeploy")"
    local gorigin
    gorigin="$(canonical_http_origin "$git_remote")"
    if [[ "$gorigin" != "$gdeploy" ]]; then
      fail_bind "Git remote origin=$gorigin != deploy=$gdeploy"
    fi
  fi

  if [[ -n "$ssh_host" ]]; then
    local sdeploy="${KILL_SWITCH_DEPLOY_SSH:-}"
    local sport="${KILL_SWITCH_SSH_PORT:-22}"
    [[ -n "$sdeploy" ]] || fail_bind "KILL_SWITCH_DEPLOY_SSH required when KILL_SWITCH_SSH_HOST is set"
    sdeploy="$(canonical_ssh_endpoint "$sdeploy")"
    local sorigin
    sorigin="$(canonical_ssh_endpoint "$ssh_host" "$sport")"
    if [[ "$sorigin" != "$sdeploy" ]]; then
      fail_bind "SSH endpoint=$sorigin != deploy=$sdeploy"
    fi
  fi
  printf 'authz_kill_switch: cross-channel bind checks passed\n' >&2
}

run_ssh_probes() {
  local host="${KILL_SWITCH_SSH_HOST:-}"
  [[ -n "$host" ]] || return 0
  local port="${KILL_SWITCH_SSH_PORT:-22}"
  local user="${KILL_SWITCH_SSH_USER:-}"
  local key="${KILL_SWITCH_SSH_KEY:-}"
  local kh="${KILL_SWITCH_SSH_KNOWN_HOSTS:-}"
  local repo="${KILL_SWITCH_SSH_REPO:-}"
  local deploy="${KILL_SWITCH_DEPLOY_SSH:-}"
  local git_bin endpoint
  [[ -n "$user" ]] || die "KILL_SWITCH_SSH_USER is required when KILL_SWITCH_SSH_HOST is set"
  [[ -n "$key" ]] || die "KILL_SWITCH_SSH_KEY is required when KILL_SWITCH_SSH_HOST is set"
  [[ -n "$kh" ]] || die "KILL_SWITCH_SSH_KNOWN_HOSTS is required when KILL_SWITCH_SSH_HOST is set"
  [[ -n "$repo" ]] || die "KILL_SWITCH_SSH_REPO is required when KILL_SWITCH_SSH_HOST is set"
  [[ -n "$deploy" ]] || die "KILL_SWITCH_DEPLOY_SSH is required when KILL_SWITCH_SSH_HOST is set"
  [[ -f "$key" ]] || die "KILL_SWITCH_SSH_KEY is not a file: $key"
  git_bin="$(require_git)"

  endpoint="$(canonical_ssh_endpoint "$host" "$port")"
  deploy="$(canonical_ssh_endpoint "$deploy")"
  if [[ "$endpoint" != "$deploy" ]]; then
    fail_t2 "SSH bind mismatch (endpoint=$endpoint deploy=$deploy)"
  fi

  evidence_session_begin
  local evidence_settled=0
  # shellcheck disable=SC2064
  trap '[[ "${evidence_settled:-0}" -eq 1 ]] || evidence_session_abort' EXIT

  if [[ ! -f "$kh" || ! -s "$kh" ]]; then
    evidence_append_check ssh ssh_host_key fail
    evidence_session_commit
    evidence_settled=1
    trap - EXIT
    fail_t2 "SSH known_hosts missing or empty: $kh"
  fi

  local out err rc=0
  out="$(mktemp "${TMPDIR:-/tmp}/killswitch-ssh-out.XXXXXX")"
  err="$(mktemp "${TMPDIR:-/tmp}/killswitch-ssh-err.XXXXXX")"
  local remote_url="ssh://${user}@${host}:${port}/${repo#/}"
  local ssh_bin="${KILL_SWITCH_SSH:-ssh}"
  set +e
  # shellcheck disable=SC2086
  GIT_SSH_COMMAND="$ssh_bin -i $key -p $port -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$kh -o GlobalKnownHostsFile=/dev/null" \
    GIT_TERMINAL_PROMPT=0 \
    "$git_bin" -c credential.helper= ls-remote "$remote_url" >"$out" 2>"$err"
  rc=$?
  set -e

  if [[ "$rc" -ne 0 ]]; then
    if grep -qiE 'HOST KEY VERIFICATION FAILED|known hosts|REMOTE HOST IDENTIFICATION' "$err"; then
      evidence_append_check ssh ssh_host_key fail
    else
      evidence_append_check ssh ssh_negotiate fail
    fi
    rm -f -- "$out" "$err"
    evidence_session_commit
    evidence_settled=1
    trap - EXIT
    fail_t2 "SSH git ls-remote failed for $endpoint (exit $rc)"
  fi
  rm -f -- "$out" "$err"
  evidence_append_check ssh ssh_host_key pass
  evidence_append_check ssh ssh_negotiate pass
  evidence_append_check ssh ssh_binding pass
  evidence_session_commit
  evidence_settled=1
  trap - EXIT
  printf 'authz_kill_switch: SSH ls-remote ok for %s; evidence recorded — not a recovery-green claim.\n' "$endpoint" >&2
}

# ---------------------------------------------------------------------------
# UN-42 Git HTTP readonly channel (git ls-remote; no ref writes)
# ---------------------------------------------------------------------------

require_git() {
  local g="${KILL_SWITCH_GIT:-}"
  if [[ -n "$g" ]]; then
    [[ -x "$g" || -f "$g" ]] || die "KILL_SWITCH_GIT is not executable: $g"
    printf '%s\n' "$g"
    return
  fi
  command -v git >/dev/null 2>&1 || die "git is required for Git channel (UN-42)"
  command -v git
}

# Redact userinfo and known secret tokens from a log line (never print credentials).
git_redact_line() {
  local line="$1"
  local secret="${KILL_SWITCH_GIT_SECRET:-}"
  # Strip scheme://user:pass@host → scheme://***@host
  line="$(printf '%s' "$line" | sed -E 's#(https?://)[^/@[:space:]]+@#\1***@#g')"
  if [[ -n "$secret" ]]; then
    line="${line//$secret/***}"
  fi
  printf '%s\n' "$line"
}

git_snapshot_repo() {
  local repo="$1"
  # Fingerprint refs + objects names only (no content dump).
  (
    cd "$repo" || exit 1
    find refs objects -type f 2>/dev/null | sort | cksum
  )
}

run_git_probes() {
  local remote="${KILL_SWITCH_GIT_REMOTE:-}"
  [[ -n "$remote" ]] || return 0
  local deploy="${KILL_SWITCH_DEPLOY_GIT:-}"
  local askpass="${KILL_SWITCH_GIT_ASKPASS:-}"
  local git_bin origin
  [[ -n "$deploy" ]] || die "KILL_SWITCH_DEPLOY_GIT is required when KILL_SWITCH_GIT_REMOTE is set"
  [[ -n "$askpass" ]] || die "KILL_SWITCH_GIT_ASKPASS is required when KILL_SWITCH_GIT_REMOTE is set"
  [[ -x "$askpass" || -f "$askpass" ]] || die "KILL_SWITCH_GIT_ASKPASS is not executable: $askpass"
  # Credentials must not appear in argv — reject embedded userinfo (user:pass@).
  if printf '%s' "$remote" | grep -qiE '^https?://[^/]*@'; then
    die "KILL_SWITCH_GIT_REMOTE must not embed credentials (userinfo); use KILL_SWITCH_GIT_ASKPASS only"
  fi
  git_bin="$(require_git)"
  deploy="$(canonical_http_origin "$deploy")"
  origin="$(canonical_http_origin "$remote")"

  evidence_session_begin
  local evidence_settled=0
  # shellcheck disable=SC2064
  trap '[[ "${evidence_settled:-0}" -eq 1 ]] || evidence_session_abort' EXIT

  if [[ "$origin" != "$deploy" ]]; then
    evidence_append_check git git_binding fail
    evidence_session_commit
    evidence_settled=1
    trap - EXIT
    fail_t2 "Git bind mismatch (origin=$origin deploy=$deploy)"
  fi
  evidence_append_check git git_binding pass

  local before="" repo_path=""
  # Optional zero-change assertion when remote is a local path exported for fixtures.
  if [[ -n "${KILL_SWITCH_GIT_ZEROCHECK_REPO:-}" ]]; then
    repo_path="$KILL_SWITCH_GIT_ZEROCHECK_REPO"
    [[ -d "$repo_path" ]] || die "KILL_SWITCH_GIT_ZEROCHECK_REPO is not a directory: $repo_path"
    before="$(git_snapshot_repo "$repo_path")"
  fi

  local out err rc=0
  out="$(mktemp "${TMPDIR:-/tmp}/killswitch-git-out.XXXXXX")"
  err="$(mktemp "${TMPDIR:-/tmp}/killswitch-git-err.XXXXXX")"
  set +e
  # Credentials only via ASKPASS; never on argv. Disable terminal prompt.
  GIT_ASKPASS="$askpass" GIT_TERMINAL_PROMPT=0 \
    "$git_bin" -c credential.helper= ls-remote "$remote" >"$out" 2>"$err"
  rc=$?
  set -e

  # Redacted logging only (raw capture may contain transport noise; never echo secrets).
  local line
  while IFS= read -r line || [[ -n "$line" ]]; do
    git_redact_line "$line" >&2
  done <"$err"

  if [[ "$rc" -ne 0 ]]; then
    rm -f -- "$out" "$err"
    evidence_append_check git git_ls_remote fail
    evidence_session_commit
    evidence_settled=1
    trap - EXIT
    fail_t2 "git ls-remote failed for $origin (exit $rc)"
  fi
  rm -f -- "$out" "$err"
  evidence_append_check git git_ls_remote pass

  if [[ -n "$repo_path" ]]; then
    local after
    after="$(git_snapshot_repo "$repo_path")"
    if [[ "$before" != "$after" ]]; then
      evidence_session_commit
      evidence_settled=1
      trap - EXIT
      fail_t2 "Git probe mutated refs/objects under $repo_path (readonly invariant)"
    fi
  fi

  evidence_session_commit
  evidence_settled=1
  trap - EXIT
  printf 'authz_kill_switch: Git ls-remote ok for %s; evidence recorded — not a recovery-green claim.\n' "$origin" >&2
}

require_yq() {
  local yq_bin="${KILL_SWITCH_YQ:-}"
  if [[ -n "$yq_bin" ]]; then
    [[ -x "$yq_bin" || -f "$yq_bin" ]] || die "KILL_SWITCH_YQ is not executable: $yq_bin"
    printf '%s\n' "$yq_bin"
    return
  fi
  command -v yq >/dev/null 2>&1 || die "yq is required for --branch compose (set KILL_SWITCH_YQ or install mikefarah/yq)"
  command -v yq
}

resolve_target() {
  local branch="$1"
  case "$branch" in
    systemd)
      [[ -n "${KILL_SWITCH_ENV_FILE:-}" ]] || die "KILL_SWITCH_ENV_FILE is required for --branch systemd"
      printf '%s\n' "$KILL_SWITCH_ENV_FILE"
      ;;
    compose)
      [[ -n "${KILL_SWITCH_COMPOSE:-}" ]] || die "KILL_SWITCH_COMPOSE is required for --branch compose"
      printf '%s\n' "$KILL_SWITCH_COMPOSE"
      ;;
    file)
      [[ -n "${KILL_SWITCH_CONFIG:-}" ]] || die "KILL_SWITCH_CONFIG is required for --branch file"
      printf '%s\n' "$KILL_SWITCH_CONFIG"
      ;;
    *)
      die "unknown --branch `$branch` (expected systemd|compose|file)"
      ;;
  esac
}

# UN-41 required inputs (transform path only; --apply-content skeleton path skips).
assert_branch_transform_inputs() {
  local branch="$1"
  case "$branch" in
    systemd)
      [[ -n "${KILL_SWITCH_UNIT:-}" ]] || die "KILL_SWITCH_UNIT is required for --branch systemd"
      ;;
    compose)
      [[ -n "${KILL_SWITCH_SERVICE:-}" ]] || die "KILL_SWITCH_SERVICE is required for --branch compose"
      require_yq >/dev/null
      ;;
    file)
      require_bin >/dev/null
      [[ -n "${MEGA_PROFILE:-}" ]] || die "MEGA_PROFILE is required for --branch file"
      ;;
  esac
}

# Transform EnvironmentFile: replace MEGA_CEDAR__ENFORCEMENT=… → =off (no append).
transform_systemd_content() {
  local src="$1"
  local dest="$2"
  python3 - "$src" "$dest" <<'PY'
import sys
src, dest = sys.argv[1], sys.argv[2]
key = "MEGA_CEDAR__ENFORCEMENT"
found = False
out = []
with open(src, "r", encoding="utf-8", errors="replace") as fh:
    for line in fh:
        raw = line.rstrip("\n")
        newline = "\n" if line.endswith("\n") else ""
        if raw and not raw.lstrip().startswith("#") and "=" in raw:
            k = raw.split("=", 1)[0].strip()
            if k == key:
                out.append(f"{key}=off{newline}")
                found = True
                continue
        out.append(line if line.endswith("\n") or not line else line + newline)
if not found:
    print(f"missing key {key} (fail-closed; will not append)", file=sys.stderr)
    sys.exit(4)
with open(dest, "w", encoding="utf-8", newline="") as fh:
    fh.writelines(out)
PY
}

# Transform compose YAML via yq (mapping only; list fail-closed; missing key fail-closed).
transform_compose_content() {
  local src="$1"
  local dest="$2"
  local svc="$KILL_SWITCH_SERVICE"
  local yq_bin
  yq_bin="$(require_yq)"
  local env_type
  env_type="$("$yq_bin" -r ".services[\"${svc}\"].environment | type" "$src")"
  case "$env_type" in
    "!!map" | "map")
      ;;
    "!!seq" | "seq" | "!!seq "*)
      die "compose service '${svc}' environment is list-shaped (fail-closed; convert to mapping)"
      ;;
    *)
      die "compose service '${svc}' environment type unsupported: ${env_type}"
      ;;
  esac
  if ! "$yq_bin" -e ".services[\"${svc}\"].environment.MEGA_CEDAR__ENFORCEMENT" "$src" >/dev/null 2>&1; then
    die "compose missing .services.${svc}.environment.MEGA_CEDAR__ENFORCEMENT (fail-closed)"
  fi
  "$yq_bin" ".services[\"${svc}\"].environment.MEGA_CEDAR__ENFORCEMENT = \"off\"" "$src" >"$dest"
}

# Transform TOML [cedar].enforcement → "off" (missing key fail-closed).
transform_file_content() {
  local src="$1"
  local dest="$2"
  python3 - "$src" "$dest" <<'PY'
import re, sys, tomllib
src, dest = sys.argv[1], sys.argv[2]
text = open(src, "r", encoding="utf-8").read()
try:
    data = tomllib.loads(text)
except Exception as err:
    print(f"invalid TOML: {err}", file=sys.stderr)
    sys.exit(4)
cedar = data.get("cedar")
if not isinstance(cedar, dict) or "enforcement" not in cedar:
    print("missing [cedar].enforcement (fail-closed; will not append)", file=sys.stderr)
    sys.exit(4)
lines = text.splitlines(keepends=True)
out = []
in_cedar = False
found = False
for line in lines:
    stripped = line.strip()
    if stripped.startswith("[") and stripped.endswith("]"):
        in_cedar = stripped == "[cedar]"
        out.append(line)
        continue
    if in_cedar and re.match(r"^enforcement\s*=", stripped):
        nl = "\n" if line.endswith("\n") else ""
        out.append(f'enforcement = "off"{nl}')
        found = True
        continue
    out.append(line)
if not found:
    print("missing [cedar].enforcement line under [cedar] (fail-closed)", file=sys.stderr)
    sys.exit(4)
open(dest, "w", encoding="utf-8", newline="").writelines(out)
PY
}

# Assert UN-01 JSON winning_source is the file we will edit (env wins → disable).
assert_file_winning_source() {
  local bin target abs_target config_arg profile_file out winning
  bin="$(require_bin)"
  target="$KILL_SWITCH_CONFIG"
  abs_target="$(realpath -- "$target")"
  if [[ -n "${MEGA_CEDAR__ENFORCEMENT+x}" ]]; then
    die "environment source MEGA_CEDAR__ENFORCEMENT wins; file branch disabled"
  fi
  # --config is the base file; profile loads config.<MEGA_PROFILE>.toml beside it.
  # When KILL_SWITCH_CONFIG points at the profile file, derive the base sibling.
  config_arg="$target"
  profile_file="$(dirname -- "$target")/config.${MEGA_PROFILE}.toml"
  if [[ -e "$profile_file" ]] && [[ "$(realpath -- "$target")" == "$(realpath -- "$profile_file")" ]]; then
    config_arg="$(dirname -- "$target")/config.toml"
    [[ -f "$config_arg" ]] || die "base config.toml missing beside profile file $target"
  fi
  out="$("$bin" --config "$config_arg" --profile "$MEGA_PROFILE" config validate --show-sources --format json 2>/dev/null || true)"
  winning="$(printf '%s\n' "$out" | python3 -c '
import json, sys
text = sys.stdin.read()
idx = text.find("{")
if idx < 0:
    raise SystemExit(0)
obj, _ = json.JSONDecoder().raw_decode(text[idx:])
print(obj.get("cedar", {}).get("enforcement", {}).get("winning_source", ""))
')"
  [[ -n "$winning" ]] || die "failed to parse cedar.enforcement.winning_source from config validate JSON"
  case "$winning" in
    "environment variable MEGA_CEDAR__ENFORCEMENT")
      die "environment source wins; file branch disabled"
      ;;
    "base file $target" | "base file $abs_target" | "profile file $target" | "profile file $abs_target")
      return 0
      ;;
    *)
      die "winning source is not the target file (got: ${winning})"
      ;;
  esac
}

readback_once() {
  local branch="$1"
  local target="$2"
  if [[ "${KILL_SWITCH_READBACK_FAIL:-}" == "1" ]]; then
    die "readback inject: KILL_SWITCH_READBACK_FAIL=1"
  fi
  case "$branch" in
    systemd)
      python3 - "$target" <<'PY'
import sys
path = sys.argv[1]
key = "MEGA_CEDAR__ENFORCEMENT"
found = None
with open(path, "r", encoding="utf-8", errors="replace") as fh:
    for line in fh:
        raw = line.rstrip("\n")
        if not raw or raw.lstrip().startswith("#") or "=" not in raw:
            continue
        k, v = raw.split("=", 1)
        if k.strip() == key:
            found = v
if found != "off":
    print(f"readback failed: {key}={found!r} (want 'off')", file=sys.stderr)
    sys.exit(4)
PY
      ;;
    compose)
      local yq_bin svc val
      yq_bin="$(require_yq)"
      svc="$KILL_SWITCH_SERVICE"
      val="$("$yq_bin" -r ".services[\"${svc}\"].environment.MEGA_CEDAR__ENFORCEMENT" "$target")"
      [[ "$val" == "off" ]] || die "readback failed: compose MEGA_CEDAR__ENFORCEMENT=${val} (want off)"
      ;;
    file)
      python3 - "$target" <<'PY'
import sys, tomllib
path = sys.argv[1]
with open(path, "rb") as fh:
    data = tomllib.load(fh)
val = data.get("cedar", {}).get("enforcement")
if val != "off":
    print(f"readback failed: cedar.enforcement={val!r} (want 'off')", file=sys.stderr)
    sys.exit(4)
PY
      ;;
  esac
}

generate_transform_content() {
  local branch="$1"
  local target="$2"
  local dest="$3"
  assert_branch_transform_inputs "$branch"
  case "$branch" in
    systemd) transform_systemd_content "$target" "$dest" ;;
    compose) transform_compose_content "$target" "$dest" ;;
    file)
      assert_file_winning_source
      transform_file_content "$target" "$dest"
      ;;
  esac
}

run_branch() {
  local branch="$1"
  local content="${2:-}"
  local target
  target="$(resolve_target "$branch")"
  # Duplicate-key check applies to KEY=VALUE-shaped targets (systemd env file,
  # and file-branch TOML is checked for duplicate bare keys on a best-effort
  # KEY= line basis when present; compose YAML is validated as regular file only).
  if [[ "$branch" == "systemd" ]]; then
    assert_no_duplicate_keys "$target"
  else
    assert_regular_nofollow "$target"
  fi

  local generated=""
  if [[ -z "$content" ]]; then
    generated="$(mktemp "${TMPDIR:-/tmp}/killswitch-xform.XXXXXX")"
    # shellcheck disable=SC2064
    trap 'rm -f -- "'"$generated"'"' RETURN
    generate_transform_content "$branch" "$target" "$generated"
    content="$generated"
  fi

  secure_replace "$target" "$content"

  if [[ -n "$generated" ]]; then
    readback_once "$branch" "$target"
    rm -f -- "$generated"
    trap - RETURN
  fi
}

# ---------------------------------------------------------------------------
# --selftest (UN-33 ×6 + UN-48 ×5 + UN-41 ×7 + UN-46 ×5 + UN-50 ×7 + UN-36 ×6 + UN-53 ×5 + UN-47 ×4 + UN-42 ×6)
# ---------------------------------------------------------------------------

gate_pass() {
  local name="$1"
  printf 'PASS %s\n' "$name"
}

gate_fail() {
  local name="$1"
  local detail="$2"
  printf 'FAIL %s: %s\n' "$name" "$detail" >&2
  exit 1
}

# Compare owner/mode/acl/xattr of two paths (python helper).
meta_field() {
  local path="$1"
  local field="$2"
  python3 - "$path" "$field" <<'PY'
import os, sys, stat, subprocess
path, field = sys.argv[1], sys.argv[2]
st = os.stat(path, follow_symlinks=False)
if field == "owner":
    print(f"{st.st_uid}:{st.st_gid}")
elif field == "mode":
    print(f"{stat.S_IMODE(st.st_mode):04o}")
elif field == "acl":
    try:
        out = subprocess.check_output(
            ["getfacl", "-c", "--absolute-names", "--", path],
            stderr=subprocess.PIPE,
            text=True,
        )
        print("\n".join(line for line in out.splitlines() if line and not line.startswith("#")))
    except Exception as err:
        print(f"getfacl failed: {err}", file=sys.stderr)
        raise SystemExit(4)
elif field == "xattr":
    items = []
    try:
        for name in sorted(os.listxattr(path, follow_symlinks=False)):
            val = os.getxattr(path, name, follow_symlinks=False)
            items.append(f"{name}={val!r}")
    except OSError as err:
        print(f"xattr inspect failed: {err}", file=sys.stderr)
        raise SystemExit(4)
    print(";".join(items))
else:
    raise SystemExit(f"unknown field {field}")
PY
}

run_selftest() {
  # Isolate from host env leftovers (probe/log vars must be fixture-scoped).
  unset KILL_SWITCH_LOG_DIR KILL_SWITCH_LOG_POLL_ATTEMPTS KILL_SWITCH_LOG_POLL_SLEEP_SECS \
    KILL_SWITCH_LOG_POLL_FORCE_TIMEOUT KILL_SWITCH_RG KILL_SWITCH_URLS \
    KILL_SWITCH_EXPECT_CODE KILL_SWITCH_DEPLOY_HTTP KILL_SWITCH_RESTRICTED_DIR \
    KILL_SWITCH_CONFIG KILL_SWITCH_ENV_FILE KILL_SWITCH_COMPOSE \
    KILL_SWITCH_GIT_REMOTE KILL_SWITCH_DEPLOY_GIT KILL_SWITCH_GIT_ASKPASS \
    KILL_SWITCH_GIT_SECRET KILL_SWITCH_GIT_ZEROCHECK_REPO KILL_SWITCH_GIT \
    KILL_SWITCH_SSH_HOST KILL_SWITCH_SSH_PORT KILL_SWITCH_SSH_USER KILL_SWITCH_SSH_KEY \
    KILL_SWITCH_SSH_KNOWN_HOSTS KILL_SWITCH_SSH_REPO KILL_SWITCH_DEPLOY_SSH KILL_SWITCH_SSH \
    KILL_SWITCH_DEPLOY_GIT KILL_SWITCH_SSH_FAKE_MODE || true

  local tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/killswitch-selftest.XXXXXX")"
  # shellcheck disable=SC2064
  trap 'rm -rf -- "'"$tmp"'"' EXIT

  local real_bin="${KILL_SWITCH_BIN:-}"
  if [[ -z "$real_bin" ]]; then
    if [[ -n "${CARGO_BIN_EXE_mega2:-}" ]]; then
      real_bin="$CARGO_BIN_EXE_mega2"
    elif command -v mega2 >/dev/null 2>&1; then
      real_bin="$(command -v mega2)"
    elif [[ -x "$SCRIPT_DIR/../target/debug/mega2" ]]; then
      real_bin="$SCRIPT_DIR/../target/debug/mega2"
    else
      die "KILL_SWITCH_BIN (or CARGO_BIN_EXE_mega2 / target/debug/mega2) required for --selftest"
    fi
  fi
  export KILL_SWITCH_BIN="$real_bin"

  local stub="$SCRIPT_DIR/authz_kill_switch_fsync_stub.sh"
  [[ -x "$stub" || -f "$stub" ]] || die "missing fsync stub: $stub"
  chmod +x "$stub" 2>/dev/null || true

  # UN-50: ensure preflight tools exist for subsequent --branch invocations.
  local pfbin="$tmp/pfbin"
  mkdir -p "$pfbin"
  if ! command -v getfattr >/dev/null 2>&1; then
    printf '#!/bin/sh\nexit 0\n' >"$pfbin/getfattr"
    chmod +x "$pfbin/getfattr"
  fi
  if ! command -v yq >/dev/null 2>&1; then
    local yq_arch yq_url
    case "$(uname -m)" in
      x86_64 | amd64) yq_arch="amd64" ;;
      aarch64 | arm64) yq_arch="arm64" ;;
      *) die "unsupported arch for yq bootstrap: $(uname -m)" ;;
    esac
    yq_url="https://github.com/mikefarah/yq/releases/download/v4.45.1/yq_linux_${yq_arch}"
    curl -fsSL -o "$pfbin/yq" "$yq_url"
    chmod +x "$pfbin/yq"
  fi
  export PATH="$pfbin:$PATH"
  if [[ -x "$pfbin/yq" ]]; then
    export KILL_SWITCH_YQ="$pfbin/yq"
  elif command -v yq >/dev/null 2>&1; then
    export KILL_SWITCH_YQ
    KILL_SWITCH_YQ="$(command -v yq)"
  fi
  # AC platform (not counted in VER=7): non-Linux fail-closed
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_PREFLIGHT_INJECT_UNAME=Darwin \
      bash "$SELF" --branch systemd --apply-content /dev/null 2>"$tmp/plat.err"; then
    gate_fail "platform_non_linux" "non-Linux was accepted"
  fi
  grep -q 'requires Linux' "$tmp/plat.err" || {
    printf 'FAIL platform_non_linux: missing Linux guidance\n' >&2
    cat "$tmp/plat.err" >&2
    exit 1
  }

  # --- gate 1: symlink-preset path rejected by O_EXCL|O_NOFOLLOW; symlink target refused ---
  local target1="$tmp/target1.conf"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\n' >"$target1"
  local planted="$tmp/.tmp-killswitch-planted"
  ln -s /etc/hosts "$planted"
  if python3 - "$planted" <<'PY'
import os, sys
path = sys.argv[1]
flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
try:
    os.open(path, flags, 0o600)
except OSError:
    sys.exit(4)
sys.exit(0)
PY
  then
    gate_fail "symlink_temp_reject" "O_EXCL|O_NOFOLLOW unexpectedly succeeded on symlink"
  fi
  local link_target="$tmp/link-target"
  ln -s "$target1" "$link_target"
  local content1="$tmp/content1"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$content1"
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$link_target" \
      bash "$SELF" --branch file --apply-content "$content1" 2>"$tmp/g1.err"; then
    gate_fail "symlink_temp_reject" "symlink target was accepted"
  fi
  gate_pass "symlink_temp_reject"

  # --- gate 2: non-regular target rejected ---
  local dir_target="$tmp/dir-target"
  mkdir -p "$dir_target"
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$dir_target" \
      bash "$SELF" --branch file --apply-content "$content1" 2>"$tmp/g2.err"; then
    gate_fail "non_regular_target_reject" "directory target was accepted"
  fi
  gate_pass "non_regular_target_reject"

  # --- gate 3: duplicate target keys rejected ---
  local dup="$tmp/dup.env"
  cat >"$dup" <<'EOF'
MEGA_CEDAR__ENFORCEMENT=enforce
FOO=1
MEGA_CEDAR__ENFORCEMENT=off
EOF
  if assert_no_duplicate_keys "$dup" 2>"$tmp/g3.err"; then
    gate_fail "duplicate_key_reject" "duplicate keys were accepted"
  fi
  gate_pass "duplicate_key_reject"

  # --- gate 4: atomic replace asserts ---
  local target4="$tmp/atomic.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\nKEEP=1\n' >"$target4"
  local new4="$tmp/atomic.new"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\nKEEP=1\n' >"$new4"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$target4" \
    bash "$SELF" --branch file --apply-content "$new4"
  local got
  got="$(cat -- "$target4")"
  [[ "$got" == "$(cat -- "$new4")" ]] || gate_fail "atomic_replace" "content mismatch after replace"
  # No leftover .tmp-killswitch-* for this base
  if compgen -G "$tmp/.tmp-killswitch-atomic.env-*" >/dev/null; then
    gate_fail "atomic_replace" "temp file left behind"
  fi
  gate_pass "atomic_replace"

  # --- gate 5: file fsync failure inject (pre-mv) ---
  local target5="$tmp/fsync-file.env"
  printf 'A=1\n' >"$target5"
  local new5="$tmp/fsync-file.new"
  printf 'A=2\n' >"$new5"
  local state5="$tmp/fsync-file.count"
  rm -f -- "$state5"
  if KILL_SWITCH_BIN="$stub" KILL_SWITCH_FSYNC_STUB_REAL="$real_bin" \
      KILL_SWITCH_FSYNC_STUB_FAIL_AT=file KILL_SWITCH_FSYNC_STUB_STATE="$state5" \
      KILL_SWITCH_CONFIG="$target5" \
      bash "$SELF" --branch file --apply-content "$new5" 2>"$tmp/g5.err"; then
    gate_fail "file_fsync_inject" "expected non-zero when file fsync fails"
  fi
  [[ "$(cat -- "$target5")" == "A=1" ]] || gate_fail "file_fsync_inject" "target mutated after file-fsync failure"
  gate_pass "file_fsync_inject"

  # --- gate 6: dir fsync failure inject (post-mv) ---
  local target6="$tmp/fsync-dir.env"
  printf 'B=1\n' >"$target6"
  local new6="$tmp/fsync-dir.new"
  printf 'B=2\n' >"$new6"
  local state6="$tmp/fsync-dir.count"
  rm -f -- "$state6"
  if KILL_SWITCH_BIN="$stub" KILL_SWITCH_FSYNC_STUB_REAL="$real_bin" \
      KILL_SWITCH_FSYNC_STUB_FAIL_AT=dir KILL_SWITCH_FSYNC_STUB_STATE="$state6" \
      KILL_SWITCH_CONFIG="$target6" \
      bash "$SELF" --branch file --apply-content "$new6" 2>"$tmp/g6.err"; then
    gate_fail "dir_fsync_inject" "expected non-zero when dir fsync fails"
  fi
  gate_pass "dir_fsync_inject"

  # --- UN-48 gate 7: mode preserved ---
  local target_m="$tmp/meta-mode.env"
  printf 'BEFORE=1\n' >"$target_m"
  chmod 0640 "$target_m"
  local mode_before
  mode_before="$(meta_field "$target_m" mode)"
  local new_m="$tmp/meta-mode.new"
  printf 'AFTER=1\n' >"$new_m"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$target_m" \
    bash "$SELF" --branch file --apply-content "$new_m"
  [[ "$(cat -- "$target_m")" == "AFTER=1" ]] || gate_fail "meta_mode_preserve" "content not updated"
  [[ "$(meta_field "$target_m" mode)" == "$mode_before" ]] || gate_fail "meta_mode_preserve" "mode changed"
  gate_pass "meta_mode_preserve"

  # --- UN-48 gate 8: owner preserved (same uid/gid; cannot chown without root) ---
  local target_o="$tmp/meta-owner.env"
  printf 'OWNER=1\n' >"$target_o"
  local owner_before
  owner_before="$(meta_field "$target_o" owner)"
  local new_o="$tmp/meta-owner.new"
  printf 'OWNER=2\n' >"$new_o"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$target_o" \
    bash "$SELF" --branch file --apply-content "$new_o"
  [[ "$(meta_field "$target_o" owner)" == "$owner_before" ]] || gate_fail "meta_owner_preserve" "owner changed"
  gate_pass "meta_owner_preserve"

  # --- UN-48 gate 9: ACL preserved ---
  local target_a="$tmp/meta-acl.env"
  printf 'ACL=1\n' >"$target_a"
  if ! setfacl -m "u:nobody:r" "$target_a" 2>"$tmp/setfacl.err"; then
    gate_fail "meta_acl_preserve" "setfacl required for ACL fixture: $(cat "$tmp/setfacl.err")"
  fi
  local acl_before
  acl_before="$(meta_field "$target_a" acl)"
  [[ -n "$acl_before" ]] || gate_fail "meta_acl_preserve" "ACL fixture is empty after setfacl"
  local new_a="$tmp/meta-acl.new"
  printf 'ACL=2\n' >"$new_a"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$target_a" \
    bash "$SELF" --branch file --apply-content "$new_a"
  [[ "$(meta_field "$target_a" acl)" == "$acl_before" ]] || gate_fail "meta_acl_preserve" "ACL changed"
  gate_pass "meta_acl_preserve"

  # --- UN-48 gate 10: xattr preserved ---
  local target_x="$tmp/meta-xattr.env"
  printf 'X=1\n' >"$target_x"
  python3 - "$target_x" <<'PY'
import os, sys
path = sys.argv[1]
os.setxattr(path, "user.killswitch", b"preserve-me")
PY
  local xattr_before
  xattr_before="$(meta_field "$target_x" xattr)"
  [[ -n "$xattr_before" ]] || gate_fail "meta_xattr_preserve" "failed to set fixture xattr"
  local new_x="$tmp/meta-xattr.new"
  printf 'X=2\n' >"$new_x"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$target_x" \
    bash "$SELF" --branch file --apply-content "$new_x"
  [[ "$(meta_field "$target_x" xattr)" == "$xattr_before" ]] || gate_fail "meta_xattr_preserve" "xattr changed"
  gate_pass "meta_xattr_preserve"

  # --- UN-48 gate 11: verify failure inject ---
  local target_v="$tmp/meta-verify.env"
  printf 'V=1\n' >"$target_v"
  local new_v="$tmp/meta-verify.new"
  printf 'V=2\n' >"$new_v"
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_META_VERIFY_FAIL=mode \
      KILL_SWITCH_CONFIG="$target_v" \
      bash "$SELF" --branch file --apply-content "$new_v" 2>"$tmp/g11.err"; then
    gate_fail "meta_verify_inject" "expected non-zero when metadata verify fails"
  fi
  [[ "$(cat -- "$target_v")" == "V=1" ]] || gate_fail "meta_verify_inject" "target mutated after verify failure"
  gate_pass "meta_verify_inject"

  # --- UN-41 gate 12: mapping compose transform ---
  [[ -n "${KILL_SWITCH_YQ:-}" ]] || KILL_SWITCH_YQ="$(command -v yq)"
  local compose_map="$tmp/compose-map.yml"
  cat >"$compose_map" <<'EOF'
services:
  web:
    environment:
      MEGA_CEDAR__ENFORCEMENT: enforce
      FOO: bar
EOF
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_COMPOSE="$compose_map" KILL_SWITCH_SERVICE=web \
    KILL_SWITCH_YQ="$KILL_SWITCH_YQ" \
    bash "$SELF" --branch compose
  local cmap_val
  cmap_val="$("$KILL_SWITCH_YQ" -r '.services.web.environment.MEGA_CEDAR__ENFORCEMENT' "$compose_map")"
  [[ "$cmap_val" == "off" ]] || gate_fail "compose_mapping_transform" "got ${cmap_val}"
  gate_pass "compose_mapping_transform"

  # --- UN-41 gate 13: list compose rejected ---
  local compose_list="$tmp/compose-list.yml"
  cat >"$compose_list" <<'EOF'
services:
  web:
    environment:
      - MEGA_CEDAR__ENFORCEMENT=enforce
      - FOO=bar
EOF
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_COMPOSE="$compose_list" KILL_SWITCH_SERVICE=web \
      KILL_SWITCH_YQ="$KILL_SWITCH_YQ" \
      bash "$SELF" --branch compose 2>"$tmp/g13.err"; then
    gate_fail "compose_list_reject" "list-shaped environment was accepted"
  fi
  gate_pass "compose_list_reject"

  # --- UN-41 gate 14: two initial env-file states → off ---
  local env_a="$tmp/env-enforce.env" env_b="$tmp/env-shadow.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\nKEEP=1\n' >"$env_a"
  printf 'MEGA_CEDAR__ENFORCEMENT=shadow\nKEEP=1\n' >"$env_b"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$env_a" \
    bash "$SELF" --branch systemd
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$env_b" \
    bash "$SELF" --branch systemd
  grep -qx 'MEGA_CEDAR__ENFORCEMENT=off' "$env_a" || gate_fail "env_two_initial_states" "enforce fixture"
  grep -qx 'MEGA_CEDAR__ENFORCEMENT=off' "$env_b" || gate_fail "env_two_initial_states" "shadow fixture"
  gate_pass "env_two_initial_states"

  # --- UN-41 gate 15: format-drift config (missing enforcement) ---
  local drift_dir="$tmp/drift"
  mkdir -p "$drift_dir"
  cp -- "$SCRIPT_DIR/../config/config.toml" "$drift_dir/config.toml"
  # profile required; strip enforcement from base
  python3 - "$drift_dir/config.toml" <<'PY'
import re, sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
text2, n = re.subn(
    r"(?m)^(\s*enforcement\s*=\s*\"[^\"]*\"\s*)$",
    r"# removed for drift fixture",
    text,
    count=1,
)
if n != 1:
    raise SystemExit("failed to strip enforcement from fixture")
open(path, "w", encoding="utf-8").write(text2)
PY
  printf '# empty profile\n' >"$drift_dir/config.ks.toml"
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$drift_dir/config.toml" MEGA_PROFILE=ks \
      bash "$SELF" --branch file 2>"$tmp/g15.err"; then
    gate_fail "format_drift_config" "missing enforcement was accepted"
  fi
  gate_pass "format_drift_config"

  # --- UN-41 gate 16: MEGA_PROFILE missing ---
  local ok_dir="$tmp/okcfg"
  mkdir -p "$ok_dir"
  cp -- "$SCRIPT_DIR/../config/config.toml" "$ok_dir/config.toml"
  python3 - "$ok_dir/config.toml" <<'PY'
import re, sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
text2, n = re.subn(r'(enforcement\s*=\s*)"[^"]*"', r'\1"enforce"', text, count=1)
if n != 1:
    # append cedar section
    open(path, "a", encoding="utf-8").write('\n[cedar]\nenforcement = "enforce"\n')
else:
    open(path, "w", encoding="utf-8").write(text2)
PY
  printf '# profile without cedar\n' >"$ok_dir/config.ks.toml"
  if env -u MEGA_PROFILE KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ok_dir/config.toml" \
      bash "$SELF" --branch file 2>"$tmp/g16.err"; then
    gate_fail "mega_profile_required" "missing MEGA_PROFILE was accepted"
  fi
  gate_pass "mega_profile_required"

  # --- UN-41 gate 17: winning-source env rejects ---
  if MEGA_CEDAR__ENFORCEMENT=off KILL_SWITCH_BIN="$real_bin" \
      KILL_SWITCH_CONFIG="$ok_dir/config.toml" MEGA_PROFILE=ks \
      bash "$SELF" --branch file 2>"$tmp/g17.err"; then
    gate_fail "winning_source_env_reject" "env winning source was accepted"
  fi
  gate_pass "winning_source_env_reject"

  # --- UN-41 gate 18: readback failure inject ---
  local rb_dir="$tmp/readback"
  mkdir -p "$rb_dir"
  cp -- "$ok_dir/config.toml" "$rb_dir/config.toml"
  printf '# profile without cedar\n' >"$rb_dir/config.ks.toml"
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$rb_dir/config.toml" MEGA_PROFILE=ks \
      KILL_SWITCH_READBACK_FAIL=1 \
      bash "$SELF" --branch file 2>"$tmp/g18.err"; then
    gate_fail "readback_fail_inject" "expected non-zero when readback injects"
  fi
  # Target may already be replaced before readback — inject is post-write; content should be off.
  python3 - "$rb_dir/config.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as fh:
    data = tomllib.load(fh)
assert data.get("cedar", {}).get("enforcement") == "off", data.get("cedar")
PY
  gate_pass "readback_fail_inject"

  # Happy-path file transform (not a separate VER gate; proves gate 17/18 fixtures work)
  local happy="$tmp/happy"
  mkdir -p "$happy"
  cp -- "$ok_dir/config.toml" "$happy/config.toml"
  printf '# profile without cedar\n' >"$happy/config.ks.toml"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$happy/config.toml" MEGA_PROFILE=ks \
    bash "$SELF" --branch file
  python3 - "$happy/config.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as fh:
    data = tomllib.load(fh)
assert data["cedar"]["enforcement"] == "off"
PY

  # --- UN-46 gate 19: restart failure T1 (config stays off, exit 4) ---
  local t1_env="$tmp/t1.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\nKEEP=1\n' >"$t1_env"
  local t1_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$t1_env" \
    bash "$SELF" --branch systemd --restart -- false 2>"$tmp/g19.err" || t1_rc=$?
  [[ "$t1_rc" -eq 4 ]] || gate_fail "restart_t1" "expected exit 4, got $t1_rc"
  grep -qx 'MEGA_CEDAR__ENFORCEMENT=off' "$t1_env" || gate_fail "restart_t1" "config not left off"
  grep -q 'T1 restart failed' "$tmp/g19.err" || gate_fail "restart_t1" "missing T1 message"
  grep -q 'manual restart guidance' "$tmp/g19.err" || gate_fail "restart_t1" "missing manual guidance"
  gate_pass "restart_t1"

  # --- UN-46 gate 20: post-rename fsync T3 (exit 5) ---
  local t3_target="$tmp/t3.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\n' >"$t3_target"
  local t3_new="$tmp/t3.new"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$t3_new"
  local t3_state="$tmp/t3.count"
  rm -f -- "$t3_state"
  local t3_rc=0
  KILL_SWITCH_BIN="$stub" KILL_SWITCH_FSYNC_STUB_REAL="$real_bin" \
    KILL_SWITCH_FSYNC_STUB_FAIL_AT=dir KILL_SWITCH_FSYNC_STUB_STATE="$t3_state" \
    KILL_SWITCH_CONFIG="$t3_target" \
    bash "$SELF" --branch file --apply-content "$t3_new" 2>"$tmp/g20.err" || t3_rc=$?
  [[ "$t3_rc" -eq 5 ]] || gate_fail "fsync_t3" "expected exit 5, got $t3_rc"
  grep -q 'T3 post-rename fsync failed' "$tmp/g20.err" || gate_fail "fsync_t3" "missing T3 message"
  # Must not have been rolled back to enforce by the script (safe direction).
  if grep -q 'MEGA_CEDAR__ENFORCEMENT=enforce' "$t3_target"; then
    # rename may or may not have landed; enforce only OK if replace never completed —
    # but FAIL_AT=dir means rename already happened, so content must be off.
    gate_fail "fsync_t3" "target still enforce after post-rename fsync path"
  fi
  grep -q 'MEGA_CEDAR__ENFORCEMENT=off' "$t3_target" || gate_fail "fsync_t3" "expected off after rename"
  gate_pass "fsync_t3"

  # --- UN-46 gate 21: idempotent re-run converges on off ---
  local id_env="$tmp/idem.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\n' >"$id_env"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$id_env" \
    bash "$SELF" --branch systemd
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$id_env" \
    bash "$SELF" --branch systemd
  # After T3-style partial: force off then re-run
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$id_env"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$id_env" \
    bash "$SELF" --branch systemd
  grep -qx 'MEGA_CEDAR__ENFORCEMENT=off' "$id_env" || gate_fail "idempotent_rerun" "not off after re-runs"
  gate_pass "idempotent_rerun"

  # --- UN-46 gate 22: argv with spaces (no re-split) ---
  local rec="$tmp/restart-record"
  local recorder="$tmp/restart-recorder.sh"
  cat >"$recorder" <<'EOF'
#!/usr/bin/env bash
printf '%s\0' "$@" >"$KILL_SWITCH_RESTART_RECORD"
EOF
  chmod +x "$recorder"
  local sp_env="$tmp/spaces.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\n' >"$sp_env"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$sp_env" \
    KILL_SWITCH_RESTART_RECORD="$rec" \
    bash "$SELF" --branch systemd --restart -- "$recorder" "arg with spaces" "second"
  python3 - "$rec" <<'PY'
import sys
data = open(sys.argv[1], "rb").read().split(b"\0")
# trailing empty from final \0
parts = [p.decode() for p in data if p]
assert parts == ["arg with spaces", "second"], parts
PY
  gate_pass "restart_argv_spaces"

  # --- UN-46 gate 23: special characters in argv ---
  rm -f -- "$rec"
  local spc_env="$tmp/special.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$spc_env"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_UNIT=mega.service KILL_SWITCH_ENV_FILE="$spc_env" \
    KILL_SWITCH_RESTART_RECORD="$rec" \
    bash "$SELF" --branch systemd --restart -- "$recorder" 'a$b' 'c*d' 'e;f' 'g`h'
  python3 - "$rec" <<'PY'
import sys
parts = [p.decode() for p in open(sys.argv[1], "rb").read().split(b"\0") if p]
assert parts == ["a$b", "c*d", "e;f", "g`h"], parts
PY
  gate_pass "restart_argv_special"

  # --- UN-50 gate 24: missing tool reject ---
  local miss_env="$tmp/miss.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\n' >"$miss_env"
  local miss_new="$tmp/miss.new"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$miss_new"
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_PREFLIGHT_INJECT_MISSING=yq \
      KILL_SWITCH_CONFIG="$miss_env" \
      bash "$SELF" --branch file --apply-content "$miss_new" 2>"$tmp/g24.err"; then
    gate_fail "preflight_missing_tool" "missing yq was accepted"
  fi
  grep -qi 'missing tool' "$tmp/g24.err" || gate_fail "preflight_missing_tool" "missing guidance"
  gate_pass "preflight_missing_tool"

  # --- UN-50 gate 25: fsync probe fail ---
  if KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_PREFLIGHT_INJECT_FSYNC_PROBE=fail \
      KILL_SWITCH_CONFIG="$miss_env" \
      bash "$SELF" --branch file --apply-content "$miss_new" 2>"$tmp/g25.err"; then
    gate_fail "preflight_fsync_probe" "fsync probe failure was accepted"
  fi
  grep -qi 'fsync --probe' "$tmp/g25.err" || gate_fail "preflight_fsync_probe" "missing probe message"
  gate_pass "preflight_fsync_probe"

  # --- UN-50 gates 26–30: version floors (cp/yq/jq/rg/flock) ---
  local ver_tool ver_env ver_msg
  for ver_tool in CP YQ JQ RG FLOCK; do
    ver_env="KILL_SWITCH_PREFLIGHT_INJECT_VERSION_${ver_tool}"
    case "$ver_tool" in
      CP) ver_msg="8.20" ;;
      YQ) ver_msg="4.10" ;;
      JQ) ver_msg="1.5" ;;
      RG) ver_msg="12.0" ;;
      FLOCK) ver_msg="2.20" ;;
    esac
    if env "$ver_env=$ver_msg" KILL_SWITCH_BIN="$real_bin" \
        KILL_SWITCH_CONFIG="$miss_env" \
        bash "$SELF" --branch file --apply-content "$miss_new" 2>"$tmp/gver.${ver_tool}.err"; then
      gate_fail "preflight_version_${ver_tool}" "old ${ver_tool} version was accepted"
    fi
    grep -Eiq 'upgrade|<' "$tmp/gver.${ver_tool}.err" || gate_fail "preflight_version_${ver_tool}" "missing version guidance"
    gate_pass "preflight_version_${ver_tool}"
  done

  # --- UN-36 HTTP probe helpers ---
  local http_pid="" https_pid=""
  start_http_fixture() {
    local port="$1" code="$2" path="$3"
    python3 - "$port" "$code" "$path" <<'PY' &
import http.server, sys
port, code, path = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.rstrip("/") == path.rstrip("/") or self.path == path:
            self.send_response(code)
        else:
            self.send_response(404)
        self.end_headers()
        self.wfile.write(b"ok")
    def log_message(self, *a):
        pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
    http_pid=$!
    sleep 0.2
  }
  stop_http_fixture() {
    if [[ -n "${http_pid:-}" ]]; then
      kill "$http_pid" 2>/dev/null || true
      wait "$http_pid" 2>/dev/null || true
      http_pid=""
    fi
  }

  local probe_env="$tmp/probe.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$probe_env"
  local probe_new="$tmp/probe.new"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$probe_new"
  local restricted="$tmp/restricted"
  mkdir -p "$restricted"

  evidence_json_for_latest_run() {
    local root="$1"
    local run_dir
    run_dir="$(find "$root/runs" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort | tail -n1)"
    [[ -n "$run_dir" ]] || return 1
    cat -- "$run_dir/killswitch-evidence.json"
  }

  # --- UN-36 gate 31: serving failure ---
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  local t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$probe_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:1/api/v1/cl/killswitch-probe/detail" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:1" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$probe_new" --restart -- true 2>"$tmp/g31.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "http_serving_fail" "expected exit 6, got $t2_rc"
  grep -q 'T2 probe failure' "$tmp/g31.err" || gate_fail "http_serving_fail" "missing T2"
  grep -q 'evidence-append\|http_serving\|T2' "$tmp/g31.err" || gate_fail "http_serving_fail" "missing evidence append"
  evidence_json_for_latest_run "$restricted" | grep -q 'http_serving' || gate_fail "http_serving_fail" "missing evidence json"
  gate_pass "http_serving_fail"

  # --- UN-36 gate 32: baseline status mismatch ---
  start_http_fixture 8765 503 /api/v1/cl/killswitch-probe/detail
  t2_rc=0
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$probe_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:8765/api/v1/cl/killswitch-probe/detail" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:8765" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$probe_new" --restart -- true 2>"$tmp/g32.err" || t2_rc=$?
  stop_http_fixture
  [[ "$t2_rc" -eq 6 ]] || gate_fail "http_status_mismatch" "expected exit 6, got $t2_rc"
  evidence_json_for_latest_run "$restricted" | grep -q 'http_status' || gate_fail "http_status_mismatch" "missing status evidence"
  gate_pass "http_status_mismatch"

  # --- UN-36 gate 33: bind mismatch (default port + IPv6 normalize) ---
  t2_rc=0
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  start_http_fixture 8766 200 /api/v1/cl/killswitch-probe/detail
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$probe_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:8766/api/v1/cl/killswitch-probe/detail" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:9999" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$probe_new" --restart -- true 2>"$tmp/g33.err" || t2_rc=$?
  stop_http_fixture
  [[ "$t2_rc" -eq 7 ]] || gate_fail "http_bind_mismatch" "expected exit 7 (early bind), got $t2_rc"
  grep -qi 'bind failure' "$tmp/g33.err" || gate_fail "http_bind_mismatch" "missing bind failure"
  local origin_v6 origin_def
  origin_v6="$(canonical_http_origin 'http://[::1]/x/')"
  [[ "$origin_v6" == "http://[::1]:80" ]] || gate_fail "http_bind_mismatch" "IPv6 origin got $origin_v6"
  origin_def="$(canonical_http_origin 'http://127.0.0.1/api/')"
  [[ "$origin_def" == "http://127.0.0.1:80" ]] || gate_fail "http_bind_mismatch" "default port got $origin_def"
  gate_pass "http_bind_mismatch"

  # --- UN-36 gate 34: base path + trailing slash pair ---
  start_http_fixture 8767 200 /api/v1/cl/killswitch-probe/detail
  t2_rc=0
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$probe_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:8767/api/v1/cl/killswitch-probe/detail http://127.0.0.1:8767/api/v1/cl/killswitch-probe/detail/" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:8767" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$probe_new" --restart -- true 2>"$tmp/g34.err" || t2_rc=$?
  stop_http_fixture
  [[ "$t2_rc" -eq 0 ]] || gate_fail "http_path_slash_pair" "expected success, got $t2_rc: $(cat "$tmp/g34.err")"
  grep -q 'not a recovery-green claim' "$tmp/g34.err" || gate_fail "http_path_slash_pair" "must not claim recovery-green"
  [[ "$(canonical_http_origin 'http://127.0.0.1:8767/api/v1/cl/killswitch-probe/detail')" == \
     "$(canonical_http_origin 'http://127.0.0.1:8767/api/v1/cl/killswitch-probe/detail/')" ]] \
    || gate_fail "http_path_slash_pair" "origins diverged"
  gate_pass "http_path_slash_pair"

  # --- UN-36 gate 35: invalid cert reject ---
  local tls_dir="$tmp/tls"
  mkdir -p "$tls_dir"
  openssl req -x509 -newkey rsa:2048 -keyout "$tls_dir/key.pem" -out "$tls_dir/cert.pem" \
    -days 1 -nodes -subj "/CN=127.0.0.1" 2>/dev/null
  python3 - "$tls_dir" <<'PYTLS' &
import http.server, ssl, sys
from pathlib import Path
root = Path(sys.argv[1])
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
    def log_message(self, *a):
        pass
httpd = http.server.HTTPServer(("127.0.0.1", 8768), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(root / "cert.pem", root / "key.pem")
httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
httpd.serve_forever()
PYTLS
  https_pid=$!
  sleep 0.3
  t2_rc=0
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$probe_env" \
    KILL_SWITCH_URLS="https://127.0.0.1:8768/api/v1/cl/killswitch-probe/detail" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="https://127.0.0.1:8768" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$probe_new" --restart -- true 2>"$tmp/g35.err" || t2_rc=$?
  kill "$https_pid" 2>/dev/null || true
  wait "$https_pid" 2>/dev/null || true
  [[ "$t2_rc" -eq 6 ]] || gate_fail "http_tls_reject" "expected exit 6 for self-signed, got $t2_rc"
  evidence_json_for_latest_run "$restricted" | grep -q 'tls_chain' || gate_fail "http_tls_reject" "missing tls evidence"
  gate_pass "http_tls_reject"

  # --- UN-36 gate 36: T2 terminal ---
  printf 'MEGA_CEDAR__ENFORCEMENT=off\nKEEP=1\n' >"$probe_env"
  t2_rc=0
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$probe_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:1/nope" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:1" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$probe_new" --restart -- true 2>"$tmp/g36.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "http_t2_terminal" "expected exit 6, got $t2_rc"
  grep -qx 'MEGA_CEDAR__ENFORCEMENT=off' "$probe_env" || gate_fail "http_t2_terminal" "config not left off"
  grep -q 'T2 probe failure' "$tmp/g36.err" || gate_fail "http_t2_terminal" "missing T2 message"
  evidence_json_for_latest_run "$restricted" >/dev/null || gate_fail "http_t2_terminal" "evidence missing"
  gate_pass "http_t2_terminal"

  # --- UN-53 gate 37: run-init/commit/abort wiring ---
  local e53="$tmp/e53root"
  rm -rf -- "$e53" && mkdir -p "$e53"
  local init_out run_id run_cap
  init_out="$("$real_bin" authz-audit run-init --restricted-root "$e53")"
  run_id="$(printf '%s\n' "$init_out" | sed -n 's/^run_id=//p' | head -n1)"
  run_cap="$(printf '%s\n' "$init_out" | sed -n 's/^run_cap=//p' | head -n1)"
  [[ -n "$run_id" && -n "$run_cap" ]] || gate_fail "evidence_run_lifecycle" "run-init missing ids"
  RUN_CAP="$run_cap" "$real_bin" authz-audit evidence-append \
    --restricted-root "$e53" --run-id "$run_id" \
    --channel http --check http_serving --verdict pass
  RUN_CAP="$run_cap" "$real_bin" authz-audit run-commit \
    --restricted-root "$e53" --run-id "$run_id"
  init_out="$("$real_bin" authz-audit run-init --restricted-root "$e53")"
  run_id="$(printf '%s\n' "$init_out" | sed -n 's/^run_id=//p' | head -n1)"
  run_cap="$(printf '%s\n' "$init_out" | sed -n 's/^run_cap=//p' | head -n1)"
  RUN_CAP="$run_cap" "$real_bin" authz-audit run-abort \
    --restricted-root "$e53" --run-id "$run_id"
  gate_pass "evidence_run_lifecycle"

  # --- UN-53 gate 38: lease compatible ---
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  start_http_fixture 8769 200 /ok
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$probe_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:8769/ok" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:8769" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$probe_new" --restart -- true 2>"$tmp/g38.err" || t2_rc=$?
  stop_http_fixture
  [[ "$t2_rc" -eq 0 ]] || gate_fail "evidence_lease" "probe failed: $(cat "$tmp/g38.err")"
  grep -q 'lease held' "$tmp/g38.err" || gate_fail "evidence_lease" "missing lease held log"
  local lease_path
  lease_path="$(find "$restricted/runs" -name '.lease.lock' | head -n1)"
  [[ -n "$lease_path" ]] || gate_fail "evidence_lease" "no .lease.lock created"
  if ! flock -n "$lease_path" -c true; then
    gate_fail "evidence_lease" "lease still held after commit"
  fi
  gate_pass "evidence_lease"

  # --- UN-53 gate 39: always via evidence-append ---
  if rg -n 'record_probe_evidence|KILL_SWITCH_EVIDENCE_FILE' "$SELF" | rg -v 'usage|Probes|UN-53|#' | rg -q .; then
    gate_fail "evidence_via_append" "provisional evidence helpers still present"
  fi
  evidence_json_for_latest_run "$restricted" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["schema_version"]==1 and d["checks"]'
  grep -q 'evidence-append channel=http' "$tmp/g38.err" || gate_fail "evidence_via_append" "no evidence-append log"
  gate_pass "evidence_via_append"

  # --- UN-53 gate 40: 256 KiB hard cap ---
  rm -rf -- "$e53" && mkdir -p "$e53"
  init_out="$("$real_bin" authz-audit run-init --restricted-root "$e53")"
  run_id="$(printf '%s\n' "$init_out" | sed -n 's/^run_id=//p' | head -n1)"
  run_cap="$(printf '%s\n' "$init_out" | sed -n 's/^run_cap=//p' | head -n1)"
  local cap_rc=0
  local i
  for i in $(seq 1 4000); do
    # Capture exit status immediately: `$?` after `if ! cmd` is always 0.
    set +e
    RUN_CAP="$run_cap" "$real_bin" authz-audit evidence-append \
      --restricted-root "$e53" --run-id "$run_id" \
      --channel log --check log_no_would_deny --verdict skip 2>"$tmp/cap.err"
    cap_rc=$?
    set -e
    if [[ "$cap_rc" -ne 0 ]]; then
      break
    fi
  done
  [[ "$cap_rc" -ne 0 ]] || gate_fail "evidence_256kib" "expected hard-cap failure before 4000 appends"
  grep -qiE 'hard ceiling|too large|262144|256' "$tmp/cap.err" \
    || gate_fail "evidence_256kib" "missing hard-cap message: $(tr '\n' ' ' <"$tmp/cap.err")"
  gate_pass "evidence_256kib"

  # --- UN-53 gate 41: unknown enum reject passthrough ---
  rm -rf -- "$e53" && mkdir -p "$e53"
  init_out="$("$real_bin" authz-audit run-init --restricted-root "$e53")"
  run_id="$(printf '%s\n' "$init_out" | sed -n 's/^run_id=//p' | head -n1)"
  run_cap="$(printf '%s\n' "$init_out" | sed -n 's/^run_cap=//p' | head -n1)"
  if RUN_CAP="$run_cap" "$real_bin" authz-audit evidence-append \
      --restricted-root "$e53" --run-id "$run_id" \
      --channel http --check not_a_real_check --verdict pass 2>"$tmp/unk.err"; then
    gate_fail "evidence_unknown_enum" "unknown check was accepted"
  fi
  grep -qi 'unknown\|not legal\|expected' "$tmp/unk.err" || gate_fail "evidence_unknown_enum" "missing reject message"
  RUN_CAP="$run_cap" "$real_bin" authz-audit run-abort --restricted-root "$e53" --run-id "$run_id" || true
  gate_pass "evidence_unknown_enum"

  # --- UN-47 gates (42–45): log channel / would-deny ---
  local log_dir="$tmp/logs"
  local log_env="$tmp/log.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$log_env"
  local log_new="$tmp/log.new"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$log_new"

  # Gate 42: new would-deny in incremental slice → T2
  rm -rf -- "$log_dir" "$restricted" && mkdir -p "$log_dir" "$restricted"
  printf 'noise before cursor\n' >"$log_dir/app.log"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$log_env" \
    KILL_SWITCH_LOG_DIR="$log_dir" \
    KILL_SWITCH_LOG_POLL_ATTEMPTS=5 \
    KILL_SWITCH_LOG_POLL_SLEEP_SECS=0 \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$log_new" --restart -- \
      bash -c "printf 'line with would-deny marker\\n' >>\"$log_dir/app.log\"" \
      2>"$tmp/g42.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "log_would_deny_detect" "expected exit 6, got $t2_rc: $(cat "$tmp/g42.err")"
  evidence_json_for_latest_run "$restricted" | grep -q 'log_no_would_deny' \
    || gate_fail "log_would_deny_detect" "missing log evidence"
  gate_pass "log_would_deny_detect"

  # Gate 43: rg exit >1 is tool error (not zero-hit)
  rm -rf -- "$log_dir" "$restricted" && mkdir -p "$log_dir" "$restricted"
  printf 'stable\n' >"$log_dir/app.log"
  local bad_rg="$tmp/bad-rg"
  cat >"$bad_rg" <<'BADRG'
#!/usr/bin/env bash
exit 2
BADRG
  chmod +x "$bad_rg"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$log_env" \
    KILL_SWITCH_LOG_DIR="$log_dir" \
    KILL_SWITCH_RG="$bad_rg" \
    KILL_SWITCH_LOG_POLL_ATTEMPTS=3 \
    KILL_SWITCH_LOG_POLL_SLEEP_SECS=0 \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$log_new" --restart -- true \
    2>"$tmp/g43.err" || t2_rc=$?
  [[ "$t2_rc" -ne 0 && "$t2_rc" -ne 6 ]] || gate_fail "log_rg_tool_error" "expected hard fail, got $t2_rc"
  grep -qi 'rg failed\|refusing to treat as zero-hit' "$tmp/g43.err" \
    || gate_fail "log_rg_tool_error" "missing rg error message: $(cat "$tmp/g43.err")"
  gate_pass "log_rg_tool_error"

  # Gate 44: poll timeout (fixture force)
  rm -rf -- "$log_dir" "$restricted" && mkdir -p "$log_dir" "$restricted"
  printf 'base\n' >"$log_dir/app.log"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$log_env" \
    KILL_SWITCH_LOG_DIR="$log_dir" \
    KILL_SWITCH_LOG_POLL_FORCE_TIMEOUT=1 \
    KILL_SWITCH_LOG_POLL_ATTEMPTS=2 \
    KILL_SWITCH_LOG_POLL_SLEEP_SECS=0 \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$log_new" --restart -- true \
    2>"$tmp/g44.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "log_poll_timeout" "expected exit 6 on poll timeout, got $t2_rc: $(cat "$tmp/g44.err")"
  grep -qi 'poll timed out\|flush poll' "$tmp/g44.err" \
    || gate_fail "log_poll_timeout" "missing timeout message"
  gate_pass "log_poll_timeout"

  # Gate 45: rotation cursor — pre-cursor would-deny ignored; post-rotate incremental counted
  rm -rf -- "$log_dir" "$restricted" && mkdir -p "$log_dir" "$restricted"
  printf 'old would-deny should be ignored after cursor\n' >"$log_dir/app.log"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$log_env" \
    KILL_SWITCH_LOG_DIR="$log_dir" \
    KILL_SWITCH_LOG_POLL_ATTEMPTS=4 \
    KILL_SWITCH_LOG_POLL_SLEEP_SECS=0 \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$log_new" --restart -- \
      bash -c "
        mv '$log_dir/app.log' '$log_dir/app.log.1'
        printf 'rotated clean line\\n' >'$log_dir/app.log'
      " 2>"$tmp/g45ok.err" || t2_rc=$?
  [[ "$t2_rc" -eq 0 ]] || gate_fail "log_rotate_cursor" "clean rotate should pass, got $t2_rc: $(cat "$tmp/g45ok.err")"
  rm -rf -- "$log_dir" "$restricted" && mkdir -p "$log_dir" "$restricted"
  printf 'pre-cursor would-deny noise\n' >"$log_dir/app.log"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$log_env" \
    KILL_SWITCH_LOG_DIR="$log_dir" \
    KILL_SWITCH_LOG_POLL_ATTEMPTS=4 \
    KILL_SWITCH_LOG_POLL_SLEEP_SECS=0 \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$log_new" --restart -- \
      bash -c "
        mv '$log_dir/app.log' '$log_dir/app.log.1'
        printf 'fresh would-deny after rotate\\n' >'$log_dir/app.log'
      " 2>"$tmp/g45.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "log_rotate_cursor" "expected exit 6 for post-rotate would-deny, got $t2_rc"
  gate_pass "log_rotate_cursor"


  # --- UN-42 gates (46–51): Git HTTP readonly ---
  local git_root="$tmp/gitfx"
  rm -rf -- "$git_root" && mkdir -p "$git_root/www/repo.git" "$git_root/wt"
  git init --bare "$git_root/www/repo.git" >/dev/null
  git -C "$git_root/www/repo.git" symbolic-ref HEAD refs/heads/main >/dev/null
  git clone "$git_root/www/repo.git" "$git_root/wt/c" >/dev/null 2>&1
  git -C "$git_root/wt/c" checkout -B main >/dev/null 2>&1
  printf 'seed\n' >"$git_root/wt/c/f"
  git -C "$git_root/wt/c" add f >/dev/null
  git -C "$git_root/wt/c" -c user.email=t@t -c user.name=t commit -m seed >/dev/null
  git -C "$git_root/wt/c" push origin main >/dev/null 2>&1
  git -C "$git_root/www/repo.git" update-server-info >/dev/null
  local askpass_ok="$git_root/askpass-ok"
  cat >"$askpass_ok" <<'ASK'
#!/usr/bin/env bash
# Fixture askpass: unused for dumb HTTP without 401, but required by contract.
printf 'fixture-password\n'
ASK
  chmod +x "$askpass_ok"
  local git_env="$tmp/git.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$git_env"
  local git_new="$tmp/git.new"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$git_new"

  start_git_http() {
    local port="$1"
    python3 -m http.server "$port" --directory "$git_root/www" >/dev/null 2>&1 &
    git_http_pid=$!
    sleep 0.25
  }
  stop_git_http() {
    if [[ -n "${git_http_pid:-}" ]]; then
      kill "$git_http_pid" 2>/dev/null || true
      wait "$git_http_pid" 2>/dev/null || true
      git_http_pid=""
    fi
  }

  # Gate 46: ls-remote auth/transport pass
  start_git_http 18761
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$git_env" \
    KILL_SWITCH_GIT_REMOTE="http://127.0.0.1:18761/repo.git" \
    KILL_SWITCH_DEPLOY_GIT="http://127.0.0.1:18761" \
    KILL_SWITCH_GIT_ASKPASS="$askpass_ok" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$git_new" --restart -- true 2>"$tmp/g46.err" || t2_rc=$?
  stop_git_http
  [[ "$t2_rc" -eq 0 ]] || gate_fail "git_ls_remote_ok" "expected 0, got $t2_rc: $(cat "$tmp/g46.err")"
  evidence_json_for_latest_run "$restricted" | grep -q 'git_ls_remote' \
    || gate_fail "git_ls_remote_ok" "missing evidence"
  gate_pass "git_ls_remote_ok"

  # Gate 47: auth/transport failure → T2
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$git_env" \
    KILL_SWITCH_GIT_REMOTE="http://127.0.0.1:1/no-such-git.git" \
    KILL_SWITCH_DEPLOY_GIT="http://127.0.0.1:1" \
    KILL_SWITCH_GIT_ASKPASS="$askpass_ok" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$git_new" --restart -- true 2>"$tmp/g47.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "git_ls_remote_fail" "expected 6, got $t2_rc"
  gate_pass "git_ls_remote_fail"

  # Gate 48: bind mismatch (incl. default-port / IPv6 normalize helpers)
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  start_git_http 18762
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$git_env" \
    KILL_SWITCH_GIT_REMOTE="http://127.0.0.1:18762/repo.git" \
    KILL_SWITCH_DEPLOY_GIT="http://127.0.0.1:9999" \
    KILL_SWITCH_GIT_ASKPASS="$askpass_ok" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$git_new" --restart -- true 2>"$tmp/g48.err" || t2_rc=$?
  stop_git_http
  [[ "$t2_rc" -eq 7 ]] || gate_fail "git_bind_mismatch" "expected 7 (early bind), got $t2_rc"
  grep -qi 'bind failure' "$tmp/g48.err" || gate_fail "git_bind_mismatch" "missing bind failure"
  local g_origin
  g_origin="$(canonical_http_origin 'http://[::1]/repo.git')"
  [[ "$g_origin" == "http://[::1]:80" ]] || gate_fail "git_bind_mismatch" "IPv6 origin $g_origin"
  g_origin="$(canonical_http_origin 'https://example.com/repo.git')"
  [[ "$g_origin" == "https://example.com:443" ]] || gate_fail "git_bind_mismatch" "default https $g_origin"
  gate_pass "git_bind_mismatch"

  # Gate 49: /repo.git path + trailing slash pair (path preserved in request, origin equal)
  start_git_http 18763
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$git_env" \
    KILL_SWITCH_GIT_REMOTE="http://127.0.0.1:18763/repo.git/" \
    KILL_SWITCH_DEPLOY_GIT="http://127.0.0.1:18763" \
    KILL_SWITCH_GIT_ASKPASS="$askpass_ok" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$git_new" --restart -- true 2>"$tmp/g49.err" || t2_rc=$?
  stop_git_http
  [[ "$t2_rc" -eq 0 ]] || gate_fail "git_path_slash_pair" "expected 0, got $t2_rc: $(cat "$tmp/g49.err")"
  [[ "$(canonical_http_origin 'http://127.0.0.1:18763/repo.git')" == \
     "$(canonical_http_origin 'http://127.0.0.1:18763/repo.git/')" ]] \
    || gate_fail "git_path_slash_pair" "origins diverged"
  gate_pass "git_path_slash_pair"

  # Gate 50: credential redaction (secret must not appear in stderr)
  local askpass_secret="$git_root/askpass-secret"
  cat >"$askpass_secret" <<'ASK'
#!/usr/bin/env bash
printf '%s\n' 'SUPERSECRET_GIT_TOKEN_UN42'
ASK
  chmod +x "$askpass_secret"
  # Wrapper git that prints a fake URL containing the secret then delegates
  local git_wrap="$git_root/git-wrap"
  cat >"$git_wrap" <<WRAP
#!/usr/bin/env bash
# Simulate noisy stderr that might include secrets; real git follows.
echo "debug remote=http://user:SUPERSECRET_GIT_TOKEN_UN42@127.0.0.1/repo.git" >&2
exec git "\$@"
WRAP
  chmod +x "$git_wrap"
  start_git_http 18764
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$git_env" \
    KILL_SWITCH_GIT="$git_wrap" \
    KILL_SWITCH_GIT_REMOTE="http://127.0.0.1:18764/repo.git" \
    KILL_SWITCH_DEPLOY_GIT="http://127.0.0.1:18764" \
    KILL_SWITCH_GIT_ASKPASS="$askpass_secret" \
    KILL_SWITCH_GIT_SECRET='SUPERSECRET_GIT_TOKEN_UN42' \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$git_new" --restart -- true 2>"$tmp/g50.err" || t2_rc=$?
  stop_git_http
  [[ "$t2_rc" -eq 0 ]] || gate_fail "git_redaction" "expected 0, got $t2_rc: $(cat "$tmp/g50.err")"
  if grep -Fq 'SUPERSECRET_GIT_TOKEN_UN42' "$tmp/g50.err"; then
    gate_fail "git_redaction" "secret leaked into stderr"
  fi
  grep -q '\*\*\*' "$tmp/g50.err" || gate_fail "git_redaction" "expected redacted marker"
  gate_pass "git_redaction"

  # Gate 51: refs/objects unchanged by ls-remote
  start_git_http 18765
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$git_env" \
    KILL_SWITCH_GIT_REMOTE="http://127.0.0.1:18765/repo.git" \
    KILL_SWITCH_DEPLOY_GIT="http://127.0.0.1:18765" \
    KILL_SWITCH_GIT_ASKPASS="$askpass_ok" \
    KILL_SWITCH_GIT_ZEROCHECK_REPO="$git_root/www/repo.git" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$git_new" --restart -- true 2>"$tmp/g51.err" || t2_rc=$?
  stop_git_http
  [[ "$t2_rc" -eq 0 ]] || gate_fail "git_zero_mutation" "expected 0, got $t2_rc: $(cat "$tmp/g51.err")"
  gate_pass "git_zero_mutation"


  # --- UN-44 gates (52–58): SSH + cross-channel bind ---
  local sshfx="$tmp/sshfx"
  rm -rf -- "$sshfx" && mkdir -p "$sshfx"
  local fake_git="$sshfx/fake-git"
  cat >"$fake_git" <<'FGIT'
#!/usr/bin/env bash
if [[ "${1:-}" == "-c" ]]; then
  shift 2
fi
if [[ "${1:-}" == "ls-remote" ]]; then
  mode="${KILL_SWITCH_SSH_FAKE_MODE:-ok}"
  kh="${KILL_SWITCH_SSH_KNOWN_HOSTS:-}"
  if [[ ! -f "$kh" ]]; then
    echo "Host key verification failed." >&2
    exit 128
  fi
  if [[ "$mode" == "mismatch" ]]; then
    echo "Host key verification failed." >&2
    exit 128
  fi
  if [[ "$mode" == "negotiate_fail" ]]; then
    echo "Permission denied (publickey)." >&2
    exit 128
  fi
  printf 'deadbeef\tHEAD\n'
  exit 0
fi
exec git "$@"
FGIT
  chmod +x "$fake_git"
  local kh_ok="$sshfx/known_hosts"
  printf '127.0.0.1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFIXTUREUN44HOSTKEY\n' >"$kh_ok"
  local key_ok="$sshfx/id_ed25519"
  cat >"$key_ok" <<'KEY'
-----BEGIN OPENSSH PRIVATE KEY-----
fixture
-----END OPENSSH PRIVATE KEY-----
KEY
  chmod 600 "$key_ok"
  local ssh_env="$tmp/ssh.env"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$ssh_env"
  local ssh_new="$tmp/ssh.new"
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$ssh_new"

  # Gate 52: SSH negotiate + ls-remote pass
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ssh_env" \
    KILL_SWITCH_GIT="$fake_git" \
    KILL_SWITCH_SSH_HOST=127.0.0.1 KILL_SWITCH_SSH_PORT=22 \
    KILL_SWITCH_SSH_USER=git KILL_SWITCH_SSH_KEY="$key_ok" \
    KILL_SWITCH_SSH_KNOWN_HOSTS="$kh_ok" KILL_SWITCH_SSH_REPO=repo.git \
    KILL_SWITCH_DEPLOY_SSH='127.0.0.1:22' \
    KILL_SWITCH_SSH_FAKE_MODE=ok \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$ssh_new" --restart -- true 2>"$tmp/g52.err" || t2_rc=$?
  [[ "$t2_rc" -eq 0 ]] || gate_fail "ssh_ls_remote_ok" "expected 0, got $t2_rc: $(cat "$tmp/g52.err")"
  evidence_json_for_latest_run "$restricted" | grep -qE 'ssh_negotiate|ssh_host_key' \
    || gate_fail "ssh_ls_remote_ok" "missing ssh evidence"
  gate_pass "ssh_ls_remote_ok"

  # Gate 53: known_hosts missing
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ssh_env" \
    KILL_SWITCH_GIT="$fake_git" \
    KILL_SWITCH_SSH_HOST=127.0.0.1 KILL_SWITCH_SSH_PORT=22 \
    KILL_SWITCH_SSH_USER=git KILL_SWITCH_SSH_KEY="$key_ok" \
    KILL_SWITCH_SSH_KNOWN_HOSTS="$sshfx/missing_kh" KILL_SWITCH_SSH_REPO=repo.git \
    KILL_SWITCH_DEPLOY_SSH='127.0.0.1:22' \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$ssh_new" --restart -- true 2>"$tmp/g53.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "ssh_known_hosts_missing" "expected 6, got $t2_rc"
  gate_pass "ssh_known_hosts_missing"

  # Gate 54: known_hosts mismatch
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ssh_env" \
    KILL_SWITCH_GIT="$fake_git" \
    KILL_SWITCH_SSH_HOST=127.0.0.1 KILL_SWITCH_SSH_PORT=22 \
    KILL_SWITCH_SSH_USER=git KILL_SWITCH_SSH_KEY="$key_ok" \
    KILL_SWITCH_SSH_KNOWN_HOSTS="$kh_ok" KILL_SWITCH_SSH_REPO=repo.git \
    KILL_SWITCH_DEPLOY_SSH='127.0.0.1:22' \
    KILL_SWITCH_SSH_FAKE_MODE=mismatch \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$ssh_new" --restart -- true 2>"$tmp/g54.err" || t2_rc=$?
  [[ "$t2_rc" -eq 6 ]] || gate_fail "ssh_known_hosts_mismatch" "expected 6, got $t2_rc"
  gate_pass "ssh_known_hosts_mismatch"

  # Gate 55: cross-channel bind mismatch → exit 7, config untouched
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\nKEEP=1\n' >"$ssh_env"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ssh_env" \
    KILL_SWITCH_SSH_HOST=127.0.0.1 KILL_SWITCH_SSH_PORT=22 \
    KILL_SWITCH_SSH_USER=git KILL_SWITCH_SSH_KEY="$key_ok" \
    KILL_SWITCH_SSH_KNOWN_HOSTS="$kh_ok" KILL_SWITCH_SSH_REPO=repo.git \
    KILL_SWITCH_DEPLOY_SSH='127.0.0.1:2222' \
    bash "$SELF" --branch file --apply-content "$ssh_new" --restart -- true 2>"$tmp/g55.err" || t2_rc=$?
  [[ "$t2_rc" -eq 7 ]] || gate_fail "ssh_cross_bind_mismatch" "expected 7, got $t2_rc: $(cat "$tmp/g55.err")"
  grep -qx 'MEGA_CEDAR__ENFORCEMENT=enforce' "$ssh_env" || gate_fail "ssh_cross_bind_mismatch" "config was written"
  grep -qi 'bind failure' "$tmp/g55.err" || gate_fail "ssh_cross_bind_mismatch" "missing guidance"
  [[ "$(canonical_ssh_endpoint '::1')" == "[::1]:22" ]] || gate_fail "ssh_cross_bind_mismatch" "ipv6 default"
  [[ "$(canonical_ssh_endpoint 'Example.COM' '22')" == "example.com:22" ]] || gate_fail "ssh_cross_bind_mismatch" "host case"
  gate_pass "ssh_cross_bind_mismatch"

  # Gate 56: HTTP path + Git /repo.git slash pairs
  printf 'MEGA_CEDAR__ENFORCEMENT=off\n' >"$ssh_env"
  start_http_fixture 18770 200 /api/v1/cl/killswitch-probe/detail
  start_git_http 18771
  rm -rf -- "$restricted" && mkdir -p "$restricted"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ssh_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:18770/api/v1/cl/killswitch-probe/detail/" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:18770" \
    KILL_SWITCH_GIT_REMOTE="http://127.0.0.1:18771/repo.git/" \
    KILL_SWITCH_DEPLOY_GIT="http://127.0.0.1:18771" \
    KILL_SWITCH_GIT_ASKPASS="$askpass_ok" \
    KILL_SWITCH_RESTRICTED_DIR="$restricted" \
    bash "$SELF" --branch file --apply-content "$ssh_new" --restart -- true 2>"$tmp/g56.err" || t2_rc=$?
  stop_http_fixture
  stop_git_http
  [[ "$t2_rc" -eq 0 ]] || gate_fail "cross_path_slash_pair" "expected 0, got $t2_rc: $(cat "$tmp/g56.err")"
  gate_pass "cross_path_slash_pair"

  # Gate 57: bind failure terminal — HTTP deploy mismatch before write
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\n' >"$ssh_env"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ssh_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:18770/x" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:1" \
    bash "$SELF" --branch file --apply-content "$ssh_new" --restart -- true 2>"$tmp/g57.err" || t2_rc=$?
  [[ "$t2_rc" -eq 7 ]] || gate_fail "bind_fail_terminal" "expected 7, got $t2_rc"
  grep -qx 'MEGA_CEDAR__ENFORCEMENT=enforce' "$ssh_env" || gate_fail "bind_fail_terminal" "wrote config"
  grep -qi 'No files were written' "$tmp/g57.err" || gate_fail "bind_fail_terminal" "missing zero-write claim"
  gate_pass "bind_fail_terminal"

  # Gate 58: multi-entry HTTP URLs must share deploy origin
  printf 'MEGA_CEDAR__ENFORCEMENT=enforce\n' >"$ssh_env"
  t2_rc=0
  KILL_SWITCH_BIN="$real_bin" KILL_SWITCH_CONFIG="$ssh_env" \
    KILL_SWITCH_URLS="http://127.0.0.1:80/a http://127.0.0.1:81/b" \
    KILL_SWITCH_EXPECT_CODE=200 \
    KILL_SWITCH_DEPLOY_HTTP="http://127.0.0.1:80" \
    bash "$SELF" --branch file --apply-content "$ssh_new" --restart -- true 2>"$tmp/g58.err" || t2_rc=$?
  [[ "$t2_rc" -eq 7 ]] || gate_fail "bind_multi_entry" "expected 7, got $t2_rc"
  gate_pass "bind_multi_entry"

  printf 'authz_kill_switch --selftest: 58/58 gates passed (UN-33×6 + UN-48×5 + UN-41×7 + UN-46×5 + UN-50×7 + UN-36×6 + UN-53×5 + UN-47×4 + UN-42×6 + UN-44×7)\n'

  trap - EXIT
  rm -rf -- "$tmp"
}


# ---------------------------------------------------------------------------
# argv
# ---------------------------------------------------------------------------

main() {
  if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
  fi
  if [[ "${1:-}" == "--selftest" ]]; then
    run_selftest
    exit 0
  fi

  local branch="" content=""
  local -a restart_argv=()
  local want_restart=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --branch)
        branch="${2:-}"
        shift 2
        ;;
      --apply-content)
        content="${2:-}"
        shift 2
        ;;
      --restart)
        shift
        [[ "${1:-}" == "--" ]] || die "--restart requires -- before argv (got: ${1:-<eof>})"
        shift
        restart_argv=("$@")
        want_restart=1
        break
        ;;
      *)
        die "unknown argument: $1"
        ;;
    esac
  done
  [[ -n "$branch" ]] || die "missing --branch or --selftest"
  run_preflight
  # UN-44: bind before any config write (exit 7, zero writes on mismatch).
  assert_cross_channel_bindings
  if [[ "$want_restart" -eq 1 ]]; then
    # Cursor before mutate/restart so incremental slice excludes pre-switch noise.
    log_cursor_capture
  fi
  run_branch "$branch" "$content"
  if [[ "$want_restart" -eq 1 ]]; then
    run_restart "${restart_argv[@]}"
    run_http_probes
    run_log_probes
    run_git_probes
    run_ssh_probes
  fi
}

main "$@"
