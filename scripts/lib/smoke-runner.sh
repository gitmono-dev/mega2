#!/usr/bin/env bash
# Shared case runner for the plan-20261001 storage-only smoke scripts
# (oci_client / artifacts / libra). Contract: ADR-BB-04 §3/§4.
#
#   source scripts/lib/smoke-runner.sh
#   smoke_init "oci client"            # summary label
#   require_tools curl jq             # exit 2 when a tool is missing
#   run_case "OCI ping" case_ping      # default-stack case
#   optin_case "OCI x (none)" MEGA2_SMOKE_AUTH_NONE case_x
#   skip_case "OCI y" "reason"         # registered but not runnable here
#   finish                             # summary + exit code
#
# Case functions run in a subshell with `set -e` in effect, so any failing
# command fails the case (no `|| return 1` bookkeeping needed).
#
# Output: `==> <case>`, then `PASS: <case>` / `FAIL: <case>` /
# `SKIP: <case> (<reason>)`; last line
# `<label> smoke storage_only summary: <N> passed, <M> failed (<K> skipped)`.
# Exit: 0 = no failure (and, when MEGA2_SMOKE_CASE is set, that case passed);
# 1 = any failure; 2 = unmatched MEGA2_SMOKE_CASE, missing tool, or a named
# opt-in case whose switch is not set. Cases filtered out by MEGA2_SMOKE_CASE
# are not counted.

SMOKE_LABEL=""
SMOKE_CASE_FILTER="${MEGA2_SMOKE_CASE:-}"
SMOKE_CASE_HIT=0
SMOKE_PASS=0
SMOKE_FAIL=0
SMOKE_SKIP=0

smoke_init() {
    SMOKE_LABEL="$1"
}

# smoke_die <message>: exit 2; the summary stays the last line of output.
smoke_die() {
    echo "ERROR: $*" >&2
    print_summary
    exit 2
}

require_tools() {
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || smoke_die "required tool not found: $tool"
    done
}

# smoke_selected <name>: true when the case should be considered at all.
smoke_selected() {
    [ -z "$SMOKE_CASE_FILTER" ] || [ "$SMOKE_CASE_FILTER" = "$1" ]
}

run_case() {
    local name="$1" fn="$2" rc=0 had_errexit=0
    smoke_selected "$name" || return 0
    SMOKE_CASE_HIT=1
    echo "==> $name"
    # Not inside an `if`/`||` condition: that would disable `set -e` in the
    # case body and let `false; true` pass.
    case $- in *e*) had_errexit=1 ;; esac
    set +e
    ( set -e; "$fn" )
    rc=$?
    if [ "$had_errexit" -eq 1 ]; then set -e; fi
    if [ "$rc" -eq 0 ]; then
        SMOKE_PASS=$((SMOKE_PASS + 1))
        echo "PASS: $name"
    else
        SMOKE_FAIL=$((SMOKE_FAIL + 1))
        echo "FAIL: $name" >&2
    fi
}

# skip_case <name> <reason>: a registered case that cannot run in this
# environment. Counted as skipped; naming it in MEGA2_SMOKE_CASE exits 2.
skip_case() {
    local name="$1" reason="$2"
    smoke_selected "$name" || return 0
    SMOKE_CASE_HIT=1
    if [ -n "$SMOKE_CASE_FILTER" ]; then
        smoke_die "case '$name' requested but cannot run: $reason"
    fi
    SMOKE_SKIP=$((SMOKE_SKIP + 1))
    echo "==> $name"
    echo "SKIP: $name ($reason)"
}

# optin_case <name> <switch-var> <fn>: runs only when ${switch-var}=1.
optin_case() {
    local name="$1" switch="$2" fn="$3"
    smoke_selected "$name" || return 0
    SMOKE_CASE_HIT=1
    if [ "${!switch:-0}" != "1" ]; then
        if [ -n "$SMOKE_CASE_FILTER" ]; then
            smoke_die "opt-in case '$name' requested but $switch=1 is not set"
        fi
        SMOKE_SKIP=$((SMOKE_SKIP + 1))
        echo "==> $name"
        echo "SKIP: $name ($switch not set)"
        return 0
    fi
    run_case "$name" "$fn"
}

print_summary() {
    echo "$SMOKE_LABEL smoke storage_only summary: $SMOKE_PASS passed, $SMOKE_FAIL failed ($SMOKE_SKIP skipped)"
}

# require_case_filter_hit: exit 2 when MEGA2_SMOKE_CASE matched no case.
require_case_filter_hit() {
    if [ -n "$SMOKE_CASE_FILTER" ] && [ "$SMOKE_CASE_HIT" -eq 0 ]; then
        smoke_die "MEGA2_SMOKE_CASE did not match any case: $SMOKE_CASE_FILTER"
    fi
}

finish() {
    require_case_filter_hit
    print_summary
    if [ "$SMOKE_FAIL" -gt 0 ]; then
        exit 1
    fi
    exit 0
}

# redact <token>: stdin -> stdout with the literal token replaced by *** and
# every URL query removed (signed URLs carry credentials in the query).
redact() {
    TOK="${1:-}" perl -pe 'BEGIN{$t=$ENV{TOK}} s/\Q$t\E/***/g if length $t; s{(https?://[^\s"?]+)\?[^\s"]*}{$1?***}g'
}
