#!/usr/bin/env bash
# authz_kill_switch.sh — Kill Switch skeleton + secure write + metadata preserve
# (UN-33 / UN-48).
#
# REL-02 family child: production and fixtures share this file. Later cards add
# transforms (UN-41), restart/T1/T3 (UN-46), preflight (UN-50), and recovery
# probes (UN-36/47/42/44) on the same --selftest entry.
#
# Modes:
#   --branch systemd|compose|file [--apply-content <file>]
#       Resolve the branch target, validate it, then atomically replace its
#       bytes from --apply-content (UN-41 will generate that content).
#   --selftest
#       Run UN-33 skeleton gates + UN-48 metadata gates (no --branch).
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
authz_kill_switch.sh — Kill Switch skeleton + metadata preserve (UN-33/UN-48).

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
# --selftest (UN-33 ×6 + UN-48 ×5)
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

  printf 'authz_kill_switch --selftest: 11/11 gates passed (UN-33×6 + UN-48×5)\n'
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
