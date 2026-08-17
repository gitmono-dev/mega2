#!/usr/bin/env bash
# authz_kill_switch_fsync_stub.sh — fsync failure injector for UN-33 selftest.
#
# Acts as KILL_SWITCH_BIN. Forwards everything to KILL_SWITCH_FSYNC_STUB_REAL
# except `authz-audit fsync <path>` calls, which are counted:
#   FAIL_AT=file → non-zero on the 1st path fsync (pre-mv)
#   FAIL_AT=dir  → forward the 1st, non-zero on the 2nd (post-mv)
#
# State file: ${KILL_SWITCH_FSYNC_STUB_STATE:-/tmp/killswitch-fsync-stub.count}

set -euo pipefail

REAL="${KILL_SWITCH_FSYNC_STUB_REAL:-}"
FAIL_AT="${KILL_SWITCH_FSYNC_STUB_FAIL_AT:-}"
STATE="${KILL_SWITCH_FSYNC_STUB_STATE:-${TMPDIR:-/tmp}/killswitch-fsync-stub.count}"

[[ -n "$REAL" ]] || {
  printf 'fsync stub: KILL_SWITCH_FSYNC_STUB_REAL is required\n' >&2
  exit 4
}

is_path_fsync() {
  # Exact shape: authz-audit fsync <PATH>  (not --probe)
  [[ $# -ge 3 && "$1" == "authz-audit" && "$2" == "fsync" && "$3" != "--probe" ]]
}

if is_path_fsync "$@"; then
  count=0
  if [[ -f "$STATE" ]]; then
    count="$(cat -- "$STATE")"
  fi
  count=$((count + 1))
  printf '%s\n' "$count" >"$STATE"

  case "$FAIL_AT" in
    file)
      if [[ "$count" -eq 1 ]]; then
        printf 'fsync stub: injecting file-fsync failure (call %s)\n' "$count" >&2
        exit 4
      fi
      ;;
    dir)
      if [[ "$count" -eq 2 ]]; then
        printf 'fsync stub: injecting dir-fsync failure (call %s)\n' "$count" >&2
        exit 4
      fi
      ;;
    *)
      printf 'fsync stub: KILL_SWITCH_FSYNC_STUB_FAIL_AT must be file|dir\n' >&2
      exit 4
      ;;
  esac
fi

exec "$REAL" "$@"
