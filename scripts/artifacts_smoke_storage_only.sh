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
require_tools curl jq sha256sum mktemp date rm perl head od tr wc mkdir cut

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

# art_oid: a random RFC 4122 version-4 UUID (artifact object ids are UUIDs).
art_oid() {
    local hex variant
    hex=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    variant=$(printf '%x' $(( (0x${hex:16:1} & 0x3) | 0x8 )))
    printf '%s-%s-4%s-%s%s-%s\n' "${hex:0:8}" "${hex:8:4}" "${hex:13:3}" "$variant" "${hex:17:3}" "${hex:20:12}"
}
# The push token reaches curl through a 0600 header file, never argv.
ART_AUTH_HEADER="$WORK/auth.header"
(umask 077; printf 'Authorization: Bearer %s\n' "$TOKEN" > "$ART_AUTH_HEADER") || smoke_die "cannot write the auth header file"
# art_post <endpoint> <json file> <out file>: authenticated JSON POST below
# $ART_API; prints the HTTP status.
art_post() {
    art_curl -H @"$ART_AUTH_HEADER" -H 'Content-Type: application/json' --data @"$2" -o "$3" -w '%{http_code}' "$ART_API/$1"
}
# art_set_id: a run-scoped artifact_set_id (ADR-BB-07).
art_set_id() {
    printf 'set-%s-%s\n' "$RUN_ID" "$(art_oid)"
}
# art_origin <url>: scheme://authority of a URL, redacted for messages.
art_origin() {
    local rest="${1#*://}"
    printf '%s://%s\n' "${1%%://*}" "${rest%%[/?#]*}" | redact "$TOKEN"
}
# art_presigned <url>: the URL's query (fragment ignored) carries SigV4 query
# authentication with a non-empty signature.
art_presigned() {
    local url="${1%%#*}" query
    query="${url#*\?}"
    [ "$query" != "$url" ] || return 1
    case "&$query&" in *"&X-Amz-Algorithm=AWS4-HMAC-SHA256&"*) ;; *) return 1 ;; esac
    case "&$query&" in *"&X-Amz-Signature="[!\&]*) return 0 ;; *) return 1 ;; esac
}
# art_batch_body <ns> <path> <oid> <size> and art_commit_body <ns> <path>
# <oid> <size> <artifact_set_id>: one-object request bodies (object_type
# snapshot).
art_batch_body() {
    jq -n --arg ns "$1" --arg path "$2" --arg oid "$3" --argjson size "$4" \
        '{namespace: $ns, object_type: "snapshot", intent: "upload", objects: [{path: $path, oid: $oid, size: $size}]}'
}
art_commit_body() {
    jq -n --arg ns "$1" --arg path "$2" --arg oid "$3" --argjson size "$4" --arg set "$5" \
        '{namespace: $ns, object_type: "snapshot", artifact_set_id: $set, files: [{path: $path, oid: $oid, size: $size}]}'
}

# --- BB-32 ART presigned upload and commit ------------------------------
case_art_presigned_upload_commit() {
    local d="$WORK/bb32" ns="bb32-$RUN_ID" path="bb32/$RUN_ID/obj.txt" oid size code href ctype set_id
    mkdir -p "$d"
    art_case_begin
    oid=$(art_oid)
    set_id=$(art_set_id)
    printf 'bb32 presigned upload %s\n' "$RUN_ID" > "$d/obj"
    size=$(wc -c < "$d/obj" | tr -d ' ')
    art_batch_body "$ns" "$path" "$oid" "$size" > "$d/batch.json"
    code=$(art_post batch "$d/batch.json" "$d/batch.out")
    [ "$code" = 200 ] || { echo "batch returned $code" >&2; return 1; }
    jq -e '.objects[0].exists == false' "$d/batch.out" > /dev/null || { echo "batch reported exists=$(art_show .objects[0].exists "$d/batch.out") for a new oid, want false" >&2; return 1; }
    href=$(jq -r '.objects[0].actions.upload.href // empty' "$d/batch.out")
    [ -n "$href" ] || { echo "batch returned no presigned upload href" >&2; return 1; }
    [ "$(art_origin "$href")" = http://127.0.0.1:29000 ] || { echo "upload href host is $(art_origin "$href"), want http://127.0.0.1:29000" >&2; return 1; }
    art_presigned "$href" || { echo "upload href carries no SigV4 query signature" >&2; return 1; }
    ctype=$(jq -r '.objects[0].actions.upload.header["Content-Type"] // "application/octet-stream"' "$d/batch.out")
    [[ "$ctype" =~ ^[A-Za-z0-9.+-]+/[A-Za-z0-9.+-]+$ ]] || { echo "upload Content-Type header is not a plain media type" >&2; return 1; }
    code=$(art_curl -X PUT -H "Content-Type: $ctype" --data-binary @"$d/obj" -o /dev/null -w '%{http_code}' "$href")
    [[ "$code" =~ ^2[0-9][0-9]$ ]] || { echo "presigned PUT returned $code" >&2; return 1; }
    art_commit_body "$ns" "$path" "$oid" "$size" "$set_id" > "$d/commit.json"
    code=$(art_post commit "$d/commit.json" "$d/commit.out")
    [ "$code" = 200 ] || { echo "commit returned $code" >&2; return 1; }
    jq -e '.status == "ok"' "$d/commit.out" > /dev/null || { echo "commit status is $(art_show .status "$d/commit.out"), want \"ok\"" >&2; return 1; }
    jq -e --arg id "$set_id" '.artifact_set_id == $id' "$d/commit.out" > /dev/null || { echo "commit returned artifact_set_id $(art_show .artifact_set_id "$d/commit.out"), want the run-scoped id it sent" >&2; return 1; }
    code=$(art_curl -o "$d/sets.json" -w '%{http_code}' "$ART_API/sets?namespace=$ns&object_type=snapshot")
    [ "$code" = 200 ] || { echo "anonymous GET sets returned $code" >&2; return 1; }
    jq -e --arg id "$set_id" 'any(.sets[]; .artifact_set_id == $id)' "$d/sets.json" > /dev/null || { echo "anonymous GET sets does not list the committed set" >&2; return 1; }
}

# art_server_put <oid> <file>: authenticated fallback PUT objects/{oid};
# prints the HTTP status.
art_server_put() {
    art_curl -H @"$ART_AUTH_HEADER" -X PUT -H 'Content-Type: application/octet-stream' --data-binary @"$2" \
        -o /dev/null -w '%{http_code}' "$ART_API/objects/$1"
}

# --- BB-33 ART server PUT upload and commit -----------------------------
case_art_server_put_commit() {
    local d="$WORK/bb33" ns="bb33-$RUN_ID" path="bb33/$RUN_ID/obj.txt" oid size code set_id
    mkdir -p "$d"
    art_case_begin
    oid=$(art_oid)
    set_id=$(art_set_id)
    printf 'bb33 server put %s\n' "$RUN_ID" > "$d/obj"
    size=$(wc -c < "$d/obj" | tr -d ' ')
    code=$(art_server_put "$oid" "$d/obj")
    [ "$code" = 204 ] || { echo "fallback PUT returned $code" >&2; return 1; }
    art_batch_body "$ns" "$path" "$oid" "$size" > "$d/batch.json"
    code=$(art_post batch "$d/batch.json" "$d/batch.out")
    [ "$code" = 200 ] || { echo "batch returned $code" >&2; return 1; }
    jq -e '.objects[0].exists == true' "$d/batch.out" > /dev/null || { echo "batch reported exists=$(art_show .objects[0].exists "$d/batch.out") after the fallback PUT, want true" >&2; return 1; }
    art_commit_body "$ns" "$path" "$oid" "$size" "$set_id" > "$d/commit.json"
    code=$(art_post commit "$d/commit.json" "$d/commit.out")
    [ "$code" = 200 ] || { echo "commit returned $code" >&2; return 1; }
    jq -e '.status == "ok"' "$d/commit.out" > /dev/null || { echo "commit status is $(art_show .status "$d/commit.out"), want \"ok\"" >&2; return 1; }
    jq -e --arg id "$set_id" '.artifact_set_id == $id' "$d/commit.out" > /dev/null || { echo "commit returned artifact_set_id $(art_show .artifact_set_id "$d/commit.out"), want the run-scoped id it sent" >&2; return 1; }
}

# art_seed <ns> <path> <file> [<metadata json>] [<artifact_set_id>]: upload
# <file> through the fallback PUT and commit it as a one-file snapshot set
# under a run-scoped artifact_set_id (generated unless given); prints
# "<oid> <artifact_set_id>".
art_seed() {
    local oid size code out set_id
    out="$WORK/seed.$RANDOM$RANDOM"
    oid=$(art_oid)
    set_id="${5:-$(art_set_id)}"
    size=$(wc -c < "$3" | tr -d ' ')
    code=$(art_server_put "$oid" "$3")
    [ "$code" = 204 ] || { echo "seed fallback PUT returned $code" >&2; return 1; }
    art_commit_body "$1" "$2" "$oid" "$size" "$set_id" \
        | jq --argjson meta "${4:-null}" 'if $meta == null then . else .metadata = $meta end' > "$out.json" || return 1
    code=$(art_post commit "$out.json" "$out.out")
    [ "$code" = 200 ] || { echo "seed commit returned $code" >&2; return 1; }
    jq -e '.status == "ok"' "$out.out" > /dev/null || { echo "seed commit status is $(art_show .status "$out.out"), want \"ok\"" >&2; return 1; }
    jq -e --arg id "$set_id" '.artifact_set_id == $id' "$out.out" > /dev/null || { echo "seed commit returned another artifact_set_id" >&2; return 1; }
    printf '%s %s\n' "$oid" "$set_id"
}

# art_download <object url> <out file>: GET without following, require a
# 302 whose Location is a SigV4-presigned URL on http://127.0.0.1:29000, then
# fetch exactly that Location (no further redirects) into <out file>.
art_download() {
    local code location
    art_curl -o /dev/null -w '%{http_code} %{redirect_url}\n' "$1" > "$2.redirect" || return 1
    read -r code location < "$2.redirect"
    [ "$code" = 302 ] || { echo "GET of the object without following returned $code, want 302" >&2; return 1; }
    [ "$(art_origin "$location")" = http://127.0.0.1:29000 ] || { echo "Location host is $(art_origin "$location"), want http://127.0.0.1:29000" >&2; return 1; }
    art_presigned "$location" || { echo "Location carries no SigV4 query signature" >&2; return 1; }
    code=$(art_curl -o "$2" -w '%{http_code}' "$location") || return 1
    [ "$code" = 200 ] || { echo "GET of the validated Location returned $code" >&2; return 1; }
}

# --- BB-34 ART presigned download ---------------------------------------
case_art_presigned_download() {
    local d="$WORK/bb34" ns="bb34-$RUN_ID" seed oid code href
    mkdir -p "$d"
    art_case_begin
    head -c 4096 /dev/urandom > "$d/obj"
    seed=$(art_seed "$ns" "bb34/$RUN_ID/obj.bin" "$d/obj") || return 1
    read -r oid _ <<< "$seed"
    art_download "$ART_API/objects/$oid" "$d/got" || return 1
    [ "$(sha256sum < "$d/got" | cut -d' ' -f1)" = "$(sha256sum < "$d/obj" | cut -d' ' -f1)" ] || { echo "downloaded bytes differ from the upload" >&2; return 1; }
    code=$(art_curl -o "$d/link.json" -w '%{http_code}' "$ART_API/objects/$oid?mode=link")
    [ "$code" = 200 ] || { echo "mode=link returned $code" >&2; return 1; }
    href=$(jq -r '.actions.download.href | strings' "$d/link.json")
    [ -n "$href" ] || { echo "mode=link JSON has no actions.download.href string" >&2; return 1; }
    [ "$(art_origin "$href")" = http://127.0.0.1:29000 ] || { echo "mode=link href host is $(art_origin "$href"), want http://127.0.0.1:29000" >&2; return 1; }
    art_presigned "$href" || { echo "mode=link href carries no SigV4 query signature" >&2; return 1; }
}

# --- BB-35 ART reject unauthenticated write -----------------------------
case_art_reject_unauth_write() {
    local d="$WORK/bb35" ns="bb35-$RUN_ID" path="bb35/$RUN_ID/obj.txt" oid code
    mkdir -p "$d"
    art_case_begin
    oid=$(art_oid)
    printf 'bb35\n' > "$d/obj"
    art_batch_body "$ns" "$path" "$oid" 5 > "$d/batch.json"
    code=$(art_curl -H 'Content-Type: application/json' --data @"$d/batch.json" -o /dev/null -w '%{http_code}' "$ART_API/batch")
    [ "$code" = 401 ] || { echo "batch without a token returned $code" >&2; return 1; }
    art_commit_body "$ns" "$path" "$oid" 5 "$(art_set_id)" > "$d/commit.json"
    code=$(art_curl -H 'Content-Type: application/json' --data @"$d/commit.json" -o /dev/null -w '%{http_code}' "$ART_API/commit")
    [ "$code" = 401 ] || { echo "commit without a token returned $code" >&2; return 1; }
    code=$(art_curl -X PUT -H 'Content-Type: application/octet-stream' --data-binary @"$d/obj" -o /dev/null -w '%{http_code}' "$ART_API/objects/$oid")
    [ "$code" = 401 ] || { echo "fallback PUT without a token returned $code" >&2; return 1; }
}

# --- BB-36 ART commit with missing objects ------------------------------
# Any missing object aborts the whole commit: no artifact set or manifest is
# committed, the set does not exist afterwards, and only the absent oid is
# reported.
case_art_commit_missing_objects() {
    local d="$WORK/bb36" ns="bb36-$RUN_ID" present missing set_id size code
    mkdir -p "$d"
    art_case_begin
    present=$(art_oid)
    missing=$(art_oid)
    set_id=$(art_set_id)
    printf 'bb36 present %s\n' "$RUN_ID" > "$d/obj"
    size=$(wc -c < "$d/obj" | tr -d ' ')
    code=$(art_server_put "$present" "$d/obj")
    [ "$code" = 204 ] || { echo "fallback PUT returned $code" >&2; return 1; }
    jq -n --arg ns "$ns" --arg set "$set_id" --arg p "$present" --arg m "$missing" --arg dir "bb36/$RUN_ID" --argjson size "$size" \
        '{namespace: $ns, object_type: "snapshot", artifact_set_id: $set,
          files: [{path: "\($dir)/present.txt", oid: $p, size: $size}, {path: "\($dir)/missing.txt", oid: $m, size: 7}]}' > "$d/commit.json"
    code=$(art_post commit "$d/commit.json" "$d/commit.out")
    [ "$code" = 200 ] || { echo "commit returned $code" >&2; return 1; }
    jq -e '.status == "missing_objects"' "$d/commit.out" > /dev/null || { echo "commit status is $(art_show .status "$d/commit.out"), want \"missing_objects\"" >&2; return 1; }
    jq -e --arg m "$missing" '.missing_objects == [$m]' "$d/commit.out" > /dev/null || { echo "missing_objects is $(art_show .missing_objects "$d/commit.out"), want only the never-uploaded oid" >&2; return 1; }
    code=$(art_curl -o /dev/null -w '%{http_code}' "$ART_API/sets/$set_id?namespace=$ns&object_type=snapshot")
    [ "$code" = 404 ] || { echo "GET sets/{id} for the uncommitted set returned $code" >&2; return 1; }
}

# --- BB-37 ART commit replay and conflict -------------------------------
case_art_commit_replay_conflict() {
    local d="$WORK/bb37" ns="bb37-$RUN_ID" dir="bb37/$RUN_ID" set_id oid size code
    mkdir -p "$d"
    art_case_begin
    set_id=$(art_set_id)
    printf 'bb37 %s\n' "$RUN_ID" > "$d/obj"
    read -r oid _ <<< "$(art_seed "$ns" "$dir/a.txt" "$d/obj" null "$set_id")"
    [ -n "$oid" ] || { echo "seed commit failed" >&2; return 1; }
    size=$(wc -c < "$d/obj" | tr -d ' ')
    art_commit_body "$ns" "$dir/a.txt" "$oid" "$size" "$set_id" > "$d/replay.json"
    code=$(art_post commit "$d/replay.json" "$d/replay.out")
    [ "$code" = 200 ] || { echo "replaying the same manifest returned $code" >&2; return 1; }
    jq -e '.status == "ok"' "$d/replay.out" > /dev/null || { echo "replay status is $(art_show .status "$d/replay.out"), want \"ok\"" >&2; return 1; }
    art_commit_body "$ns" "$dir/b.txt" "$oid" "$size" "$set_id" > "$d/conflict.json"
    code=$(art_post commit "$d/conflict.json" "$d/conflict.out")
    [ "$code" = 409 ] || { echo "a different manifest for the same artifact_set_id returned $code" >&2; return 1; }
}

run_case "ART discovery" case_art_discovery
run_case "ART presigned upload and commit" case_art_presigned_upload_commit
run_case "ART server PUT upload and commit" case_art_server_put_commit
run_case "ART presigned download" case_art_presigned_download
run_case "ART reject unauthenticated write" case_art_reject_unauth_write
run_case "ART commit with missing objects" case_art_commit_missing_objects
run_case "ART commit replay and conflict" case_art_commit_replay_conflict

finish
