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
require_tools curl jq oras sha256sum mktemp date rm perl

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

finish
