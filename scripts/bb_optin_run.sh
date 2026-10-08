#!/usr/bin/env bash
# Host-side helper that runs one opt-in smoke case on a temporarily switched
# mega2 and always restores the default stack
# (ADR-BB-06).
#
# Usage:
#   scripts/bb_optin_run.sh <none|gc|local|views> <script> <case> <log>
#   scripts/bb_optin_run.sh --selftest <none|gc|local|views> <log>
#
# Modes (mega2 is recreated with --force-recreate):
#   none   overlay docker/docker-compose-storage-only.auth-none.yml (push_auth=none)
#   gc     --env-file config/compose.env.storage-only.artifacts-gc
#   local  --env-file config/compose.env.storage-only.local (local object storage)
#   views overlay docker/docker-compose-storage-only.views.yml (isolated views stack)
#
# The case runs inside the `interop-smoke` service with its opt-in switch set
# (MEGA2_SMOKE_AUTH_NONE / MEGA2_SMOKE_ARTIFACTS_GC /
# MEGA2_SMOKE_LOCAL_STORAGE / MEGA2_SMOKE_VIEWS)
# and its output is tee'd to <log>. After the switch, and again after an
# EXIT trap recreates mega2 with the default configuration, the helper reads
# the compose config-hash label of the running mega2 container: it must equal
# the switched configuration's hash before the case runs, and the default
# configuration's hash (recorded before the switch) after restoration.
#
# <script> must be an allowed smoke entrypoint for its mode, and a
# zero exit is only accepted when the log holds `PASS: <case>`.
#
# --selftest runs a stub instead of a smoke script: it prints
# `bb-optin-selftest-case-ran`, reads the artifacts discovery document and
# prints `bb-optin-selftest-signed_url_put=<value>`, then exits 1.
#
# Exit codes: 0 = case passed and stack restored; 1 = case failed, or the
# switch did not take effect and the case did not run, and the stack was
# restored; 2 = argument or precondition error (unknown mode, missing env
# file, stack lock held) and nothing was switched; 3 = restoration failed
# (the running mega2's config hash is not the default one, or mega2 is not
# healthy) and needs manual recovery.
#
# Test-only knobs: MEGA2_OPTIN_CONFIG_DIR overrides the env-file directory
# (default `config`); MEGA2_OPTIN_FAULT_RESTORE=hash|unhealthy forces the
# post-restore hash check or health wait to report failure, and =stale skips
# the restoring recreate so the check sees the still-switched container;
# MEGA2_OPTIN_FAULT_SWITCH=stale skips the switching recreate so the switch
# check sees the still-default container.
#
# Only one helper run may own the mega2-trunk stack at a time: a lock
# directory is held from the initial config hash through restoration.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

COMPOSE=(docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml)
CONFIG_DIR="${MEGA2_OPTIN_CONFIG_DIR:-config}"
FAULT="${MEGA2_OPTIN_FAULT_RESTORE:-}"
SWITCH_FAULT="${MEGA2_OPTIN_FAULT_SWITCH:-}"

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
    views)
        switched_args=(-f docker/docker-compose-storage-only.views.yml)
        switch_var=MEGA2_SMOKE_VIEWS
        ;;
    *)
        echo "bb-optin: unknown mode '$mode'; nothing switched" >&2
        echo "usage: $0 <none|gc|local|views> <script> <case> <log> | --selftest <mode> <log>" >&2
        exit 2
        ;;
esac
if [ "${switched_args[0]}" = "--env-file" ] && [ ! -f "${switched_args[1]}" ]; then
    echo "bb-optin: env file ${switched_args[1]} not found; nothing switched" >&2
    exit 2
fi
if [ -z "$log" ]; then
    echo "bb-optin: missing <log> argument; nothing switched" >&2
    exit 2
fi
if [ "$selftest" -eq 0 ]; then
    if [ -z "$script" ] || [ -z "$case_name" ]; then
        echo "bb-optin: <script> and <case> must both be non-empty; nothing switched" >&2
        exit 2
    fi
    case "$script" in
        oci_client_smoke_storage_only.sh|artifacts_smoke_storage_only.sh|libra_smoke_storage_only.sh|libra_view_smoke.sh) ;;
        *)
            echo "bb-optin: '$script' is not an allowed smoke entrypoint; nothing switched" >&2
            exit 2
            ;;
    esac
    if { [ "$mode" = views ] && [ "$script" != libra_view_smoke.sh ]; } \
        || { [ "$mode" != views ] && [ "$script" = libra_view_smoke.sh ]; }; then
        echo "bb-optin: script '$script' does not match mode '$mode'; nothing switched" >&2
        exit 2
    fi
    if [ ! -f "scripts/$script" ]; then
        echo "bb-optin: scripts/$script not found; nothing switched" >&2
        exit 2
    fi
fi

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
start_seconds=$SECONDS

config_hash() {
    "$@" config --hash mega2 | awk '{print $2}'
}

# Compose stores the service config hash on the container it created.
running_hash() {
    local id
    id=$("${COMPOSE[@]}" ps -q mega2) && [ -n "$id" ] || return 1
    docker inspect -f '{{index .Config.Labels "com.docker.compose.config-hash"}}' "$id"
}

default_hash=$(config_hash "${COMPOSE[@]}")
say "bb-optin: config hash before: $default_hash"

# shellcheck disable=SC2317,SC2329  # invoked indirectly via `trap restore EXIT`
restore() {
    local rc=$?
    set +e
    local up_rc=0
    if [ "$FAULT" != "stale" ]; then
        "${COMPOSE[@]}" up -d --wait --no-deps --force-recreate mega2 >> "$log" 2>&1
        up_rc=$?
    fi
    local after
    after=$(running_hash)
    if [ "$FAULT" = "unhealthy" ]; then up_rc=99; fi
    if [ "$FAULT" = "hash" ]; then after="fault-injected"; fi
    rmdir "$LOCK_DIR"
    if [ "$mode" = views ]; then
        say "bb-optin: views elapsed $((SECONDS - start_seconds))s"
    fi
    if [ "$up_rc" -eq 0 ] && [ "$after" = "$default_hash" ]; then
        say "bb-optin: restored mega2 (config hash match)"
        exit "$rc"
    fi
    say "bb-optin: restore FAILED (up exit $up_rc, hash $after, want $default_hash); recover with: docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml up -d --wait --force-recreate mega2"
    exit 3
}
trap restore EXIT

if [ "$mode" = views ]; then
    if ! scripts/libra_view_smoke_setup.sh reset >> "$log" 2>&1; then
        say "bb-optin: views setup reset failed"
        exit 1
    fi
fi

if [ "$SWITCH_FAULT" != "stale" ]; then
    "${COMPOSE[@]}" "${switched_args[@]}" up -d --wait --no-deps --force-recreate mega2 >> "$log" 2>&1
fi
switched_hash=$(config_hash "${COMPOSE[@]}" "${switched_args[@]}")
if [ "$switched_hash" = "$default_hash" ]; then
    say "bb-optin: switch did not change the mega2 config (hash $switched_hash)"
    exit 1
fi
running=$(running_hash || true)
if [ "$running" != "$switched_hash" ]; then
    say "bb-optin: running mega2 config hash ${running:-<none>} is not the switched $switched_hash"
    exit 1
fi
say "bb-optin: switched config hash $default_hash != $running"

if [ "$mode" = views ] && [ "$selftest" -eq 0 ]; then
    if ! scripts/libra_view_smoke_setup.sh prepare >> "$log" 2>&1; then
        say "bb-optin: views setup prepare failed"
        exit 1
    fi
fi

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
# A zero exit only counts when the named case really ran and passed.
if [ "$selftest" -eq 0 ] && [ "$case_rc" -eq 0 ] && ! grep -qxF "PASS: $case_name" "$log"; then
    say "bb-optin: '$case_name' exited 0 without a PASS line; treating as failure"
    case_rc=1
fi
exit "$case_rc"
