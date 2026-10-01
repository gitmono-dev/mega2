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
require_tools curl jq oras sha256sum mktemp date rm perl awk tr timeout rg grep head tail cut

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
# Every case has one deadline (OCI_CASE_BUDGET seconds, default 60) set by
# oci_case_begin; oci_curl and oci_t only get the time that is left.
OCI_CASE_BUDGET="${OCI_CASE_BUDGET:-60}"
oci_case_begin() { OCI_DEADLINE=$((SECONDS + OCI_CASE_BUDGET)); }
oci_left() { echo $(( ${OCI_DEADLINE:-$((SECONDS + OCI_CASE_BUDGET))} - SECONDS )); }
# oci_t <cmd...>: run a command within the remaining case budget.
oci_t() {
    local left
    left=$(oci_left)
    [ "$left" -gt 0 ] || { echo "case budget exhausted" >&2; return 124; }
    timeout "$left" "$@"
}
# oci_curl: curl with a 5 s connect timeout and the remaining case budget.
oci_curl() {
    local left
    left=$(oci_left)
    [ "$left" -gt 0 ] || { echo "case budget exhausted" >&2; return 124; }
    curl -sS --connect-timeout 5 --max-time "$left" "$@"
}
# oci_run <outfile> <cmd...>: run within the budget, keep stdout+stderr in
# <outfile>, and print it redacted when the command fails.
oci_run() {
    local out="$1"
    shift
    if oci_t "$@" > "$out" 2>&1; then
        return 0
    fi
    redact "$TOKEN" < "$out" >&2
    return 1
}
# oci_header <file> <name>: exact value of a response header (case-insensitive name).
oci_header() {
    awk -v n="$(printf '%s' "$2" | tr '[:upper:]' '[:lower:]')" \
        'tolower($1) == n":" {sub(/^[^:]*:[ \t]*/, ""); sub(/\r$/, ""); print; exit}' "$1"
}

# --- BB-11 OCI ping -----------------------------------------------------
case_oci_ping() {
    local hdr="$WORK/ping.hdr" code
    oci_case_begin
    code=$(oci_curl -D "$hdr" -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/")
    [ "$code" = 200 ] || { echo "anonymous GET /v2/ returned $code" >&2; return 1; }
    [ "$(oci_header "$hdr" docker-distribution-api-version)" = "registry/2.0" ] \
        || { echo "Docker-Distribution-API-Version is not registry/2.0" >&2; return 1; }
    code=$(oci_curl -D "$hdr" -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer bb-invalid-token' "$MEGA2_BASE_URL/v2/")
    [ "$code" = 401 ] || { echo "invalid Bearer returned $code" >&2; return 1; }
    [ -n "$(oci_header "$hdr" www-authenticate)" ] || { echo "401 without WWW-Authenticate" >&2; return 1; }
}

# --- BB-12 OCI oras push and pull ---------------------------------------
case_oci_oras_push_pull() {
    local dir="$WORK/oras" ref="$REGISTRY/bb-$RUN_ID/oras:v1" pushed fetched
    oci_case_begin
    mkdir -p "$dir/src" "$dir/out"
    export DOCKER_CONFIG="$dir/docker"
    printf 'bb oras one %s\n' "$RUN_ID" > "$dir/src/one.txt"
    printf 'bb oras two %s\n' "$RUN_ID" > "$dir/src/two.txt"
    printf '%s' "$TOKEN" | oci_run "$dir/login.out" oras login --plain-http -u bb --password-stdin "$REGISTRY"
    (cd "$dir/src" && oci_run "$dir/push.out" oras push --plain-http "$ref" one.txt two.txt)
    pushed=$(rg -o '^Digest: (sha256:[0-9a-f]{64})$' -r '$1' "$dir/push.out")
    [ "$(printf '%s\n' "$pushed" | grep -c '^sha256:')" = 1 ] || { echo "push output does not hold exactly one Digest line" >&2; return 1; }
    (cd "$dir/out" && oci_run "$dir/pull.out" oras pull --plain-http "$ref")
    for f in one.txt two.txt; do
        [ "$(sha256sum < "$dir/src/$f")" = "$(sha256sum < "$dir/out/$f")" ] \
            || { echo "pulled $f differs from the pushed file" >&2; return 1; }
    done
    oci_run "$dir/desc.json" oras manifest fetch --plain-http --descriptor "$ref"
    fetched=$(jq -r .digest "$dir/desc.json")
    [ "$pushed" = "$fetched" ] || { echo "push digest $pushed != descriptor digest $fetched" >&2; return 1; }
}

# --- BB-13 OCI reject unauthenticated push ------------------------------
case_oci_reject_unauth_push() {
    local dir="$WORK/unauth" repo="bb-$RUN_ID/unauth" code
    oci_case_begin
    mkdir -p "$dir"
    export DOCKER_CONFIG="$dir/docker"
    printf 'unauth\n' > "$dir/f.txt"
    if (cd "$dir" && oci_t oras push --debug --plain-http "$REGISTRY/$repo:v1" f.txt) > "$dir/push.out" 2>&1; then
        echo "unauthenticated oras push succeeded" >&2; return 1
    fi
    rg -q 'Response Status: "401 Unauthorized"' "$dir/push.out" || { echo "push failure did not mention 401" >&2; redact "$TOKEN" < "$dir/push.out" >&2; return 1; }
    code=$(oci_curl -o "$dir/tags.json" -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/tags/list")
    [ "$code" = 404 ] || { echo "tags/list returned $code" >&2; return 1; }
    [ "$(jq -r '.errors[0].code' "$dir/tags.json")" = NAME_UNKNOWN ] || { echo "tags/list error code is not NAME_UNKNOWN" >&2; return 1; }
}

# --- OCI helpers (introduced with BB-14) --------------------------------
# oci_abs <location>: absolute URL for a Location header value. Only paths
# under this registry's /v2/ are accepted, so the push token is never sent
# to another origin.
oci_abs() {
    case "$1" in
        /v2/*) printf '%s%s' "$MEGA2_BASE_URL" "$1" ;;
        "$MEGA2_BASE_URL"/v2/*) printf '%s' "$1" ;;
        *) echo "refusing a Location outside $MEGA2_BASE_URL/v2/ (value not shown)" >&2; return 1 ;;
    esac
}
oci_digest() { printf 'sha256:%s' "$(sha256sum < "$1" | cut -d' ' -f1)"; }

# --- BB-14 OCI chunked blob upload --------------------------------------
case_oci_chunked_upload() {
    local d="$WORK/chunked" repo="bb-$RUN_ID/chunked" code loc dg
    oci_case_begin
    mkdir -p "$d"
    head -c 3000 /dev/urandom > "$d/blob"
    head -c 1500 "$d/blob" > "$d/part1"
    tail -c 1500 "$d/blob" > "$d/part2"
    dg=$(oci_digest "$d/blob")
    code=$(oci_curl -u "bb:$TOKEN" -X POST -D "$d/h0" -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/blobs/uploads/")
    [ "$code" = 202 ] || { echo "upload init returned $code" >&2; return 1; }
    loc=$(oci_abs "$(oci_header "$d/h0" location)") || return 1
    code=$(oci_curl -u "bb:$TOKEN" -X PATCH -D "$d/h1" -o /dev/null -w '%{http_code}' \
        -H 'Content-Type: application/octet-stream' -H 'Content-Range: 0-1499' --data-binary @"$d/part1" "$loc")
    [ "$code" = 202 ] || { echo "first PATCH returned $code" >&2; return 1; }
    loc=$(oci_abs "$(oci_header "$d/h1" location)") || return 1
    code=$(oci_curl -u "bb:$TOKEN" -X PATCH -D "$d/h2" -o /dev/null -w '%{http_code}' \
        -H 'Content-Type: application/octet-stream' -H 'Content-Range: 1500-2999' --data-binary @"$d/part2" "$loc")
    [ "$code" = 202 ] || { echo "second PATCH returned $code" >&2; return 1; }
    loc=$(oci_abs "$(oci_header "$d/h2" location)") || return 1
    code=$(oci_curl -u "bb:$TOKEN" -D "$d/h3" -o /dev/null -w '%{http_code}' "$loc")
    [ "$code" = 204 ] || { echo "upload status GET returned $code" >&2; return 1; }
    [ "$(oci_header "$d/h3" range)" = "0-2999" ] || { echo "status Range is '$(oci_header "$d/h3" range)'" >&2; return 1; }
    case "$loc" in *\?*) loc="$loc&digest=$dg" ;; *) loc="$loc?digest=$dg" ;; esac
    code=$(oci_curl -u "bb:$TOKEN" -X PUT -D "$d/h4" -o /dev/null -w '%{http_code}' "$loc")
    [ "$code" = 201 ] || { echo "completing PUT returned $code" >&2; return 1; }
    [ "$(oci_header "$d/h4" docker-content-digest)" = "$dg" ] || { echo "Docker-Content-Digest mismatch" >&2; return 1; }
}

run_case "OCI ping" case_oci_ping
run_case "OCI oras push and pull" case_oci_oras_push_pull
run_case "OCI reject unauthenticated push" case_oci_reject_unauth_push
run_case "OCI chunked blob upload" case_oci_chunked_upload

finish
