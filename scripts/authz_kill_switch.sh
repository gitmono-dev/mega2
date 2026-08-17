#!/usr/bin/env bash
# authz_kill_switch.sh — Kill Switch skeleton + secure write primitives (UN-33).
#
# REL-02 family child: production and fixtures share this file. Later cards add
# transforms (UN-41), metadata preserve (UN-48), restart/T1/T3 (UN-46), preflight
# (UN-50), and recovery probes (UN-36/47/42/44) on the same --selftest entry.
#
# Modes:
#   --branch systemd|compose|file [--apply-content <file>]
#       Resolve the branch target, validate it, then atomically replace its
#       bytes from --apply-content (UN-41 will generate that content).
#   --selftest
#       Run the six UN-33 skeleton gates (no --branch).
#   -h | --help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="${BASH_SOURCE[0]}"

die() {
  printf 'authz_kill_switch: %s\n' "$*" >&2
  exit 4
}

usage() {
  cat <<'USAGE'
authz_kill_switch.sh — Kill Switch skeleton (UN-33).

Usage:
  bash scripts/authz_kill_switch.sh --branch systemd|compose|file --apply-content <file>
  bash scripts/authz_kill_switch.sh --selftest
  bash scripts/authz_kill_switch.sh -h | --help

Environment (branch mode):
  KILL_SWITCH_BIN          monoengine binary (required; used for authz-audit fsync)
  systemd: KILL_SWITCH_ENV_FILE
  compose: KILL_SWITCH_COMPOSE
  file:    KILL_SWITCH_CONFIG
USAGE
}

require_bin() {
  local bin="${KILL_SWITCH_BIN:-}"
  [[ -n "$bin" ]] || die "KILL_SWITCH_BIN is required"
  [[ -x "$bin" || -f "$bin" ]] || die "KILL_SWITCH_BIN is not executable: $bin"
  printf '%s\n' "$bin"
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

# Create a same-directory temp with O_EXCL|O_NOFOLLOW (0600), write content
# through that fd (no reopen), and print the temp path.
create_temp_excl_write() {
  local target="$1"
  local content_file="$2"
  local dir base
  dir="$(dirname -- "$target")"
  base="$(basename -- "$target")"
  python3 - "$dir" "$base" "$content_file" <<'PY'
import os, sys, secrets, stat
directory, base, content_path = sys.argv[1], sys.argv[2], sys.argv[3]

def write_all(fd, data: bytes) -> None:
    view = memoryview(data)
    while view:
        n = os.write(fd, view)
        if n <= 0:
            raise OSError("short write")
        view = view[n:]

try:
    dir_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
except OSError as err:
    print(f"open parent dir failed: {directory}: {err}", file=sys.stderr)
    sys.exit(4)

try:
    st = os.stat(base, dir_fd=dir_fd, follow_symlinks=False)
    if not stat.S_ISREG(st.st_mode):
        print(f"target must be a regular file: {base}", file=sys.stderr)
        sys.exit(4)

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

    try:
        with open(content_path, "rb") as src:
            while True:
                chunk = src.read(1024 * 1024)
                if not chunk:
                    break
                write_all(fd, chunk)
    finally:
        os.close(fd)

    print(os.path.join(directory, name))
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

# Atomic replace: write content → fsync(temp) → mv → fsync(target).
secure_replace() {
  local target="$1"
  local content_file="$2"
  local bin
  bin="$(require_bin)"

  assert_regular_nofollow "$target"
  assert_regular_nofollow "$content_file"

  local tmp
  tmp="$(create_temp_excl_write "$target" "$content_file")"
  # shellcheck disable=SC2064
  trap 'rm -f -- "'"$tmp"'"' RETURN

  fsync_path "$bin" "$tmp"
  renameat_nofollow "$tmp" "$target"
  trap - RETURN
  fsync_path "$bin" "$target"
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

run_branch() {
  local branch="$1"
  local content="${2:-}"
  [[ -n "$content" ]] || die "--apply-content <file> is required until UN-41 supplies transforms"
  local target
  target="$(resolve_target "$branch")"
  # Duplicate-key check applies to KEY=VALUE-shaped targets (systemd env file,
  # and file-branch TOML is checked for duplicate bare keys on a best-effort
  # KEY= line basis when present; compose YAML is validated as regular file only).
  if [[ "$branch" == "systemd" || "$branch" == "file" ]]; then
    assert_no_duplicate_keys "$target"
  else
    assert_regular_nofollow "$target"
  fi
  secure_replace "$target" "$content"
}

# ---------------------------------------------------------------------------
# --selftest (6 gates)
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

run_selftest() {
  local tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/killswitch-selftest.XXXXXX")"
  # shellcheck disable=SC2064
  trap 'rm -rf -- "$tmp"' EXIT

  local real_bin="${KILL_SWITCH_BIN:-}"
  if [[ -z "$real_bin" ]]; then
    if [[ -n "${CARGO_BIN_EXE_monoengine:-}" ]]; then
      real_bin="$CARGO_BIN_EXE_monoengine"
    elif command -v monoengine >/dev/null 2>&1; then
      real_bin="$(command -v monoengine)"
    elif [[ -x "$SCRIPT_DIR/../target/debug/monoengine" ]]; then
      real_bin="$SCRIPT_DIR/../target/debug/monoengine"
    else
      die "KILL_SWITCH_BIN (or CARGO_BIN_EXE_monoengine / target/debug/monoengine) required for --selftest"
    fi
  fi
  export KILL_SWITCH_BIN="$real_bin"

  local stub="$SCRIPT_DIR/authz_kill_switch_fsync_stub.sh"
  [[ -x "$stub" || -f "$stub" ]] || die "missing fsync stub: $stub"
  chmod +x "$stub" 2>/dev/null || true

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
  # Post-rename fsync failure: bytes may already be the new content; script must
  # still exit non-zero (UN-46 will freeze T3). Assert non-zero already done.
  gate_pass "dir_fsync_inject"

  printf 'authz_kill_switch --selftest: 6/6 gates passed\n'
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
      *)
        die "unknown argument: $1"
        ;;
    esac
  done
  [[ -n "$branch" ]] || die "missing --branch or --selftest"
  run_branch "$branch" "$content"
}

main "$@"
