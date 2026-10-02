#!/usr/bin/env bash
# Artifacts protocol (/api/v1/repos/{repo}/artifacts) black-box smoke for the
# storage-only compose stack (docs/plan/plan-20261001.md, Artifacts domain
# BB-30..BB-47). Runs inside the `interop-smoke` service; the client is curl
# (never libra, ADR-BB-03).
#
#   docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml --profile interop \
#     exec -T [-e MEGA2_SMOKE_CASE='<case>'] interop-smoke bash /repo/scripts/artifacts_smoke_storage_only.sh
#
# Env: MEGA2_BASE_URL (default http://127.0.0.1:9000, the runner's loopback
# relay to mega2), MEGA2_IT_SEED_TOKEN (push token), MEGA2_SMOKE_CASE
# (exact case name); opt-in cases run through scripts/bb_optin_run.sh.
# Output and exit codes: scripts/lib/smoke-runner.sh.
set -uo pipefail

# Only bash builtins until require_tools has run, so a missing tool exits 2.
SCRIPT_DIR="${BASH_SOURCE[0]%/*}"
[ "$SCRIPT_DIR" = "${BASH_SOURCE[0]}" ] && SCRIPT_DIR=.
# shellcheck source=scripts/lib/smoke-runner.sh disable=SC1091
source "$SCRIPT_DIR/lib/smoke-runner.sh"

smoke_init "artifacts"
require_tools curl jq sha256sum mktemp date rm perl head

MEGA2_BASE_URL="${MEGA2_BASE_URL:-http://127.0.0.1:9000}"
MEGA2_BASE_URL="${MEGA2_BASE_URL%/}"
TOKEN="${MEGA2_IT_SEED_TOKEN:-}"
[ -n "$TOKEN" ] || smoke_die "MEGA2_IT_SEED_TOKEN is empty"
RUN_ID="$(date -u +%Y%m%dt%H%M%S)" || smoke_die "cannot build RUN_ID"
RUN_ID="$RUN_ID$RANDOM"
# Every run writes under its own repo and namespaces (ADR-BB-07).
ART_REPO="bb-art-$RUN_ID"
ART_API="$MEGA2_BASE_URL/api/v1/repos/$ART_REPO/artifacts"
WORK="$(mktemp -d)" || smoke_die "cannot create a work directory"
trap 'rm -rf "$WORK"' EXIT

# A single-case run measures its case budget from here (see art_case_begin).
ART_RUN_START=$SECONDS

# Preflight: a completed GET of discovery (any HTTP status) means the artifacts
# API is reachable; a curl failure (refused, timeout mid-response) is not.
code=$(curl -sS --connect-timeout 3 --max-time 5 -o /dev/null -w '%{http_code}' "$ART_API/discovery" 2>/dev/null) || code=""
[[ "$code" =~ ^[1-5][0-9][0-9]$ ]] || smoke_die "$ART_API/discovery is not reachable"

export ART_REPO ART_API RUN_ID WORK TOKEN MEGA2_BASE_URL

# --- shared artifacts helpers -------------------------------------------
# Every case has one deadline set by art_case_begin: ART_CASE_BUDGET seconds
# (default 55, keeping a few seconds of the 60 s budget for local processing)
# from the case start or, when MEGA2_SMOKE_CASE names one case, from before
# the preflight, so that whole run fits the budget. art_curl only gets the
# time that is left.
ART_CASE_BUDGET="${ART_CASE_BUDGET:-55}"
art_case_begin() {
    local base=$SECONDS
    [ -z "${MEGA2_SMOKE_CASE:-}" ] || base=$ART_RUN_START
    ART_DEADLINE=$((base + ART_CASE_BUDGET))
}
art_curl() {
    local left=$(( ${ART_DEADLINE:-$((SECONDS + ART_CASE_BUDGET))} - SECONDS ))
    [ "$left" -gt 0 ] || { echo "case budget exhausted" >&2; return 124; }
    curl -sS --connect-timeout 5 --max-time "$left" "$@"
}
# art_show <jq filter> <file>: one response value for a failure message,
# compact JSON, redacted (push token, URL queries) and cut to 120 bytes.
art_show() {
    jq -c "$1" "$2" 2>/dev/null | redact "$TOKEN" | head -c 120
}

# --- BB-31 ART discovery ------------------------------------------------
case_art_discovery() {
    local out="$WORK/discovery.json" code
    art_case_begin
    code=$(art_curl -o "$out" -w '%{http_code}' "$ART_API/discovery")
    [ "$code" = 200 ] || { echo "anonymous discovery returned $code" >&2; return 1; }
    jq -e '.protocol_version == "artifacts/v1"' "$out" > /dev/null || { echo "protocol_version is $(art_show .protocol_version "$out"), want \"artifacts/v1\"" >&2; return 1; }
    jq -e '.transfers.signed_url_put == true' "$out" > /dev/null || { echo "transfers.signed_url_put is $(art_show .transfers.signed_url_put "$out"), want true" >&2; return 1; }
    jq -e '.transfers.signed_url_get == true' "$out" > /dev/null || { echo "transfers.signed_url_get is $(art_show .transfers.signed_url_get "$out"), want true" >&2; return 1; }
}

run_case "ART discovery" case_art_discovery

finish
