#!/usr/bin/env bash
# OCI Distribution (/v2) black-box smoke for the storage-only compose stack
# (docs/plan/plan-20261001.md, OCI domain BB-10..BB-23). Runs inside the
# `interop-smoke` service; clients are oras and curl (never libra, ADR-BB-03).
#
#   docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml --profile interop \
#     exec -T [-e MEGA2_SMOKE_CASE='<case>'] interop-smoke bash /repo/scripts/oci_client_smoke_storage_only.sh
#
# Env: MEGA2_BASE_URL (default http://127.0.0.1:9000, the runner's loopback
# relay to mega2), MEGA2_IT_SEED_TOKEN (push token), MEGA2_SMOKE_CASE
# (exact case name), MEGA2_SMOKE_AUTH_NONE=1 (opt-in auth-none cases; use
# scripts/bb_optin_run.sh). Output and exit codes: scripts/lib/smoke-runner.sh.
set -uo pipefail

# Only bash builtins until require_tools has run, so a missing tool exits 2.
SCRIPT_DIR="${BASH_SOURCE[0]%/*}"
[ "$SCRIPT_DIR" = "${BASH_SOURCE[0]}" ] && SCRIPT_DIR=.
# shellcheck source=scripts/lib/smoke-runner.sh disable=SC1091
source "$SCRIPT_DIR/lib/smoke-runner.sh"

smoke_init "oci client"
require_tools curl jq oras sha256sum mktemp date rm perl awk tr

MEGA2_BASE_URL="${MEGA2_BASE_URL:-http://127.0.0.1:9000}"
MEGA2_BASE_URL="${MEGA2_BASE_URL%/}"
TOKEN="${MEGA2_IT_SEED_TOKEN:-}"
[ -n "$TOKEN" ] || smoke_die "MEGA2_IT_SEED_TOKEN is empty"
REGISTRY="${MEGA2_BASE_URL#http://}"
RUN_ID="$(date -u +%Y%m%dt%H%M%S)" || smoke_die "cannot build RUN_ID"
RUN_ID="$RUN_ID$RANDOM"
WORK="$(mktemp -d)" || smoke_die "cannot create a work directory"
trap 'rm -rf "$WORK"' EXIT

# Preflight: any HTTP status from GET /v2/ means the registry is reachable.
code=$(curl -sS --connect-timeout 3 --max-time 5 -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/" 2>/dev/null || true)
[[ "$code" =~ ^[1-5][0-9][0-9]$ ]] || smoke_die "$MEGA2_BASE_URL/v2/ is not reachable"

export REGISTRY RUN_ID WORK TOKEN MEGA2_BASE_URL

# --- shared OCI helpers -------------------------------------------------
# oci_curl: curl with bounded connection and total time.
oci_curl() { curl -sS --connect-timeout 5 --max-time 30 "$@"; }
# oci_header <file> <name>: exact value of a response header (case-insensitive name).
oci_header() {
    awk -v n="$(printf '%s' "$2" | tr '[:upper:]' '[:lower:]')" \
        'tolower($1) == n":" {sub(/^[^:]*:[ \t]*/, ""); sub(/\r$/, ""); print; exit}' "$1"
}

# --- BB-11 OCI ping -----------------------------------------------------
case_oci_ping() {
    local hdr="$WORK/ping.hdr" code
    code=$(oci_curl -D "$hdr" -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/")
    [ "$code" = 200 ] || { echo "anonymous GET /v2/ returned $code" >&2; return 1; }
    [ "$(oci_header "$hdr" docker-distribution-api-version)" = "registry/2.0" ] \
        || { echo "Docker-Distribution-API-Version is not registry/2.0" >&2; return 1; }
    code=$(oci_curl -D "$hdr" -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer bb-invalid-token' "$MEGA2_BASE_URL/v2/")
    [ "$code" = 401 ] || { echo "invalid Bearer returned $code" >&2; return 1; }
    [ -n "$(oci_header "$hdr" www-authenticate)" ] || { echo "401 without WWW-Authenticate" >&2; return 1; }
}

run_case "OCI ping" case_oci_ping

finish
