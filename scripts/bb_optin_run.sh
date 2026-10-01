#!/usr/bin/env bash
# Host-side helper that runs one opt-in smoke case of plan-20261001 on a
# temporarily switched mega2 and always restores the default stack
# (ADR-BB-06).
#
# Usage:
#   scripts/bb_optin_run.sh <none|gc|local> <script> <case> <log>
#   scripts/bb_optin_run.sh --selftest <none|gc|local> <log>
#
# Modes (mega2 is recreated with --force-recreate):
#   none   overlay docker/docker-compose-storage-only.auth-none.yml (push_auth=none)
#   gc     --env-file config/compose.env.storage-only.artifacts-gc
#   local  --env-file config/compose.env.storage-only.local (local object storage)
#
# The case runs inside the `interop-smoke` service with its opt-in switch set
# (MEGA2_SMOKE_AUTH_NONE / MEGA2_SMOKE_ARTIFACTS_GC / MEGA2_SMOKE_LOCAL_STORAGE)
# and its output is tee'd to <log>. An EXIT trap recreates mega2 with the
# default configuration and compares its compose config hash with the one
# recorded before the switch.
#
# --selftest runs a stub instead of a smoke script: it prints
# `bb-optin-selftest-case-ran`, reads the artifacts discovery document and
# prints `bb-optin-selftest-signed_url_put=<value>`, then exits 1.
#
# Exit codes: 0 = case passed and stack restored; 1 = case failed and stack
# restored; 2 = another helper run holds the stack lock (nothing switched).
#
# Only one helper run may own the mega2-trunk stack at a time: a lock
# directory is held from the initial config hash through restoration.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

COMPOSE=(docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml)
CONFIG_DIR="config"

selftest=0
if [ "${1:-}" = "--selftest" ]; then
    selftest=1
    mode="${2:-}"
    script=""
    case_name="selftest"
    log="${3:-}"
else
    mode="${1:-}"
    script="${2:-}"
    case_name="${3:-}"
    log="${4:-}"
fi

switched_args=()
switch_var=""
case "$mode" in
    none)
        switched_args=(-f docker/docker-compose-storage-only.auth-none.yml)
        switch_var=MEGA2_SMOKE_AUTH_NONE
        ;;
    gc)
        switched_args=(--env-file "$CONFIG_DIR/compose.env.storage-only.artifacts-gc")
        switch_var=MEGA2_SMOKE_ARTIFACTS_GC
        ;;
    local)
        switched_args=(--env-file "$CONFIG_DIR/compose.env.storage-only.local")
        switch_var=MEGA2_SMOKE_LOCAL_STORAGE
        ;;
    *)
        echo "usage: $0 <none|gc|local> <script> <case> <log> | --selftest <mode> <log>" >&2
        exit 64
        ;;
esac

LOCK_DIR="${TMPDIR:-/tmp}/mega2-trunk-optin.lock"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    echo "bb-optin: another helper run holds $LOCK_DIR; nothing switched" >&2
    exit 2
fi
trap 'rmdir "$LOCK_DIR"' EXIT

mkdir -p "$(dirname "$log")"
umask 077
: > "$log"
chmod 600 "$log"
say() { echo "$*" | tee -a "$log"; }

config_hash() {
    "$@" config --hash mega2 | awk '{print $2}'
}

default_hash=$(config_hash "${COMPOSE[@]}")
say "bb-optin: config hash before: $default_hash"

# shellcheck disable=SC2317,SC2329  # invoked indirectly via `trap restore EXIT`
restore() {
    local rc=$?
    set +e
    "${COMPOSE[@]}" up -d --wait --no-deps --force-recreate mega2 >> "$log" 2>&1
    local up_rc=$?
    local after
    after=$(config_hash "${COMPOSE[@]}")
    if [ "$up_rc" -eq 0 ] && [ "$after" = "$default_hash" ]; then
        say "bb-optin: restored mega2 (config hash match)"
    else
        say "bb-optin: restore FAILED (up exit $up_rc, hash $after, want $default_hash)"
    fi
    rmdir "$LOCK_DIR"
    exit "$rc"
}
trap restore EXIT

"${COMPOSE[@]}" "${switched_args[@]}" up -d --wait --no-deps --force-recreate mega2 >> "$log" 2>&1
switched_hash=$(config_hash "${COMPOSE[@]}" "${switched_args[@]}")
if [ "$switched_hash" = "$default_hash" ]; then
    say "bb-optin: switch did not change the mega2 config (hash $switched_hash)"
    exit 1
fi
say "bb-optin: switched config hash $default_hash != $switched_hash"

set +e
if [ "$selftest" -eq 1 ]; then
    # shellcheck disable=SC2016  # the script body is expanded inside the container
    "${COMPOSE[@]}" --profile interop exec -T interop-smoke bash -c '
        echo bb-optin-selftest-case-ran
        v=$(curl -fsS http://127.0.0.1:9000/api/v1/repos/bb-selftest/artifacts/discovery | jq -r .transfers.signed_url_put)
        echo "bb-optin-selftest-signed_url_put=$v"
        exit 1' 2>&1 | tee -a "$log"
else
    "${COMPOSE[@]}" --profile interop exec -T -e "$switch_var=1" -e "MEGA2_SMOKE_CASE=$case_name" \
        interop-smoke bash "/repo/scripts/$script" 2>&1 | tee -a "$log"
fi
case_rc=${PIPESTATUS[0]}
set -e
exit "$case_rc"
