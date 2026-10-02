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
require_tools curl jq oras sha256sum mktemp date rm perl awk tr timeout rg grep head tail cut wc cmp

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

# oci_put_blob <repo> <file>: monolithic upload, prints the HTTP status.
oci_put_blob() {
    oci_curl -u "bb:$TOKEN" -X POST -o /dev/null -w '%{http_code}' \
        -H 'Content-Type: application/octet-stream' --data-binary @"$2" \
        "$MEGA2_BASE_URL/v2/$1/blobs/uploads/?digest=$(oci_digest "$2")"
}

# --- BB-15 OCI cross-repo blob mount ------------------------------------
case_oci_cross_repo_mount() {
    local d="$WORK/mount" src="bb-$RUN_ID/mount-src" dst="bb-$RUN_ID/mount-dst" code dg
    oci_case_begin
    mkdir -p "$d"
    head -c 512 /dev/urandom > "$d/blob"
    dg=$(oci_digest "$d/blob")
    code=$(oci_put_blob "$src" "$d/blob")
    [ "$code" = 201 ] || { echo "source upload returned $code" >&2; return 1; }
    code=$(oci_curl -u "bb:$TOKEN" -X POST -o /dev/null -w '%{http_code}' \
        "$MEGA2_BASE_URL/v2/$dst/blobs/uploads/?mount=$dg&from=$src")
    [ "$code" = 201 ] || { echo "mount from a source holding the blob returned $code" >&2; return 1; }
    code=$(oci_curl -I -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$dst/blobs/$dg")
    [ "$code" = 200 ] || { echo "HEAD of the mounted blob returned $code" >&2; return 1; }
    code=$(oci_curl -u "bb:$TOKEN" -X POST -D "$d/h" -o /dev/null -w '%{http_code}' \
        "$MEGA2_BASE_URL/v2/$dst/blobs/uploads/?mount=$dg&from=bb-$RUN_ID/mount-empty")
    [ "$code" = 202 ] || { echo "mount from a source without the blob returned $code" >&2; return 1; }
    [ -n "$(oci_header "$d/h" docker-upload-uuid)" ] || { echo "fallback session without Docker-Upload-UUID" >&2; return 1; }
}

# oci_oras_push <repo:tag> <dir> [<os/arch>]: push one small file with oras
# (as a single-platform manifest when <os/arch> is given); prints the manifest
# digest reported by the push (its single `Digest:` line).
oci_oras_push() {
    local ref="$REGISTRY/$1" dir="$2" platform="${3:-}"
    mkdir -p "$dir"
    export DOCKER_CONFIG="$dir/docker"
    printf 'bb %s %s\n' "$1" "$RUN_ID" > "$dir/payload.txt"
    printf '%s' "$TOKEN" | oci_run "$dir/login.out" oras login --plain-http -u bb --password-stdin "$REGISTRY" || return 1
    (cd "$dir" && oci_run "$dir/push.out" oras push --plain-http ${platform:+--artifact-platform "$platform"} "$ref" payload.txt) || return 1
    local pushed
    pushed=$(rg -o '^Digest: (sha256:[0-9a-f]{64})$' -r '$1' "$dir/push.out")
    [ "$(printf '%s\n' "$pushed" | grep -c '^sha256:')" = 1 ] \
        || { echo "push output does not hold exactly one Digest line" >&2; return 1; }
    printf '%s\n' "$pushed"
}

# --- BB-16 OCI manifest HEAD and conditional GET ------------------------
case_oci_manifest_head_304() {
    local d="$WORK/head" repo="bb-$RUN_ID/head" accept dg code len etag
    oci_case_begin
    accept='Accept: application/vnd.oci.image.manifest.v1+json'
    dg=$(oci_oras_push "$repo:v1" "$d")
    code=$(oci_curl -H "$accept" -o "$d/manifest.json" -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/manifests/v1")
    [ "$code" = 200 ] || { echo "GET manifest returned $code" >&2; return 1; }
    code=$(oci_curl -I -H "$accept" -D "$d/h" -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/manifests/v1")
    [ "$code" = 200 ] || { echo "HEAD manifest returned $code" >&2; return 1; }
    [ "$(oci_header "$d/h" docker-content-digest)" = "$dg" ] || { echo "HEAD digest differs from the pushed digest" >&2; return 1; }
    etag=$(oci_header "$d/h" etag)
    [ -n "$etag" ] || { echo "HEAD without ETag" >&2; return 1; }
    len=$(wc -c < "$d/manifest.json" | tr -d ' ')
    [ "$(oci_header "$d/h" content-length)" = "$len" ] || { echo "Content-Length differs from manifest size $len" >&2; return 1; }
    code=$(oci_curl -H "$accept" -H "If-None-Match: $etag" -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/manifests/v1")
    [ "$code" = 304 ] || { echo "conditional GET returned $code" >&2; return 1; }
}

# --- BB-17 OCI blob range -----------------------------------------------
case_oci_blob_range() {
    local d="$WORK/range" repo="bb-$RUN_ID/range" dg code
    oci_case_begin
    mkdir -p "$d"
    head -c 100 /dev/urandom > "$d/blob"
    dg=$(oci_digest "$d/blob")
    code=$(oci_put_blob "$repo" "$d/blob")
    [ "$code" = 201 ] || { echo "blob upload returned $code" >&2; return 1; }
    code=$(oci_curl -H 'Range: bytes=0-9' -D "$d/h" -o "$d/part" -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/blobs/$dg")
    [ "$code" = 206 ] || { echo "Range GET returned $code" >&2; return 1; }
    [ "$(oci_header "$d/h" content-range)" = "bytes 0-9/100" ] || { echo "Content-Range is '$(oci_header "$d/h" content-range)'" >&2; return 1; }
    [ "$(wc -c < "$d/part" | tr -d ' ')" = 10 ] || { echo "Range body is not 10 bytes" >&2; return 1; }
    head -c 10 "$d/blob" | cmp -s - "$d/part" || { echo "Range body differs from the first 10 blob bytes" >&2; return 1; }
    code=$(oci_curl -H 'Range: bytes=100-' -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/blobs/$dg")
    [ "$code" = 416 ] || { echo "out-of-range GET returned $code" >&2; return 1; }
}

# --- BB-18 OCI tags list pagination -------------------------------------
case_oci_tags_pagination() {
    local d="$WORK/tags" repo="bb-$RUN_ID/tags" code link
    oci_case_begin
    oci_oras_push "$repo:t1" "$d" >/dev/null
    oci_run "$d/tag.out" oras tag --plain-http "$REGISTRY/$repo:t1" t2 t3
    code=$(oci_curl -D "$d/h1" -o "$d/p1.json" -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/tags/list?n=2")
    [ "$code" = 200 ] || { echo "tags/list n=2 returned $code" >&2; return 1; }
    [ "$(jq -r .name "$d/p1.json")" = "$repo" ] || { echo "first page names another repository" >&2; return 1; }
    [ "$(jq -c .tags "$d/p1.json")" = '["t1","t2"]' ] || { echo "first page tags are $(jq -c .tags "$d/p1.json")" >&2; return 1; }
    link=$(oci_header "$d/h1" link)
    case "$link" in *'rel="next"'*) ;; *) echo "first page without a rel=\"next\" Link" >&2; return 1 ;; esac
    link=${link#<}; link=${link%%>*}
    code=$(oci_curl -D "$d/h2" -o "$d/p2.json" -w '%{http_code}' "$(oci_abs "$link")")
    [ "$code" = 200 ] || { echo "following Link returned $code" >&2; return 1; }
    [ "$(jq -r .name "$d/p2.json")" = "$repo" ] || { echo "second page names another repository" >&2; return 1; }
    [ "$(jq -c .tags "$d/p2.json")" = '["t3"]' ] || { echo "second page tags are $(jq -c .tags "$d/p2.json")" >&2; return 1; }
    if grep -qi '^link:' "$d/h2"; then echo "last page still has a Link header" >&2; return 1; fi
    code=$(oci_curl -o "$d/bad.json" -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/tags/list?n=abc")
    [ "$code" = 400 ] || { echo "n=abc returned $code" >&2; return 1; }
    [ "$(jq -r '.errors[0].code' "$d/bad.json")" = PAGINATION_NUMBER_INVALID ] || { echo "n=abc error code mismatch" >&2; return 1; }
}

# --- BB-19 OCI image index manifest -------------------------------------
case_oci_image_index() {
    local d="$WORK/index" repo="bb-$RUN_ID/index" d1 d2 s1 s2 code ct arch
    oci_case_begin
    d1=$(oci_oras_push "$repo:amd64" "$d/amd64" linux/amd64)
    d2=$(oci_oras_push "$repo:arm64" "$d/arm64" linux/arm64)
    for arch in amd64 arm64; do
        oci_run "$d/config-$arch.json" oras manifest fetch-config --plain-http "$REGISTRY/$repo:$arch"
        [ "$(jq -c '{os, architecture}' "$d/config-$arch.json")" = "{\"os\":\"linux\",\"architecture\":\"$arch\"}" ] \
            || { echo "the $arch manifest is not a linux/$arch single-platform manifest" >&2; return 1; }
    done
    oci_run "$d/desc1.json" oras manifest fetch --plain-http --descriptor "$REGISTRY/$repo:amd64"
    oci_run "$d/desc2.json" oras manifest fetch --plain-http --descriptor "$REGISTRY/$repo:arm64"
    [ "$(jq -r .digest "$d/desc1.json")" = "$d1" ] && [ "$(jq -r .digest "$d/desc2.json")" = "$d2" ] \
        || { echo "fetched descriptors differ from the pushed digests" >&2; return 1; }
    s1=$(jq -r .size "$d/desc1.json")
    s2=$(jq -r .size "$d/desc2.json")
    jq -n --arg d1 "$d1" --argjson s1 "$s1" --arg d2 "$d2" --argjson s2 "$s2" '{
        schemaVersion: 2,
        mediaType: "application/vnd.oci.image.index.v1+json",
        manifests: [
          {mediaType: "application/vnd.oci.image.manifest.v1+json", digest: $d1, size: $s1, platform: {architecture: "amd64", os: "linux"}},
          {mediaType: "application/vnd.oci.image.manifest.v1+json", digest: $d2, size: $s2, platform: {architecture: "arm64", os: "linux"}}
        ]}' > "$d/index.json"
    code=$(oci_curl -u "bb:$TOKEN" -X PUT -o /dev/null -w '%{http_code}' \
        -H 'Content-Type: application/vnd.oci.image.index.v1+json' --data-binary @"$d/index.json" \
        "$MEGA2_BASE_URL/v2/$repo/manifests/index")
    [ "$code" = 201 ] || { echo "index PUT returned $code" >&2; return 1; }
    code=$(oci_curl -H 'Accept: application/vnd.oci.image.index.v1+json' -D "$d/h" -o "$d/got.json" -w '%{http_code}' \
        "$MEGA2_BASE_URL/v2/$repo/manifests/index")
    [ "$code" = 200 ] || { echo "index GET returned $code" >&2; return 1; }
    ct=$(oci_header "$d/h" content-type)
    [ "$ct" = "application/vnd.oci.image.index.v1+json" ] || { echo "index Content-Type is '$ct'" >&2; return 1; }
    [ "$(jq '.manifests | length' "$d/got.json")" = 2 ] || { echo "index does not list 2 manifests" >&2; return 1; }
    [ "$(jq -c '[.manifests[].digest]' "$d/got.json")" = "$(jq -cn --arg a "$d1" --arg b "$d2" '[$a, $b]')" ] \
        || { echo "index manifests are not the two pushed manifests" >&2; return 1; }
    [ "$(jq -c '[.manifests[].platform | "\(.os)/\(.architecture)"]' "$d/got.json")" = '["linux/amd64","linux/arm64"]' ] \
        || { echo "index platforms are not linux/amd64 and linux/arm64" >&2; return 1; }
}

# --- BB-20 OCI reject manifest with unknown blob ------------------------
case_oci_manifest_unknown_blob() {
    local d="$WORK/unknown" repo="bb-$RUN_ID/unknown" cfg_dg cfg_size missing code
    oci_case_begin
    mkdir -p "$d"
    printf '{}' > "$d/config.json"
    code=$(oci_put_blob "$repo" "$d/config.json")
    [ "$code" = 201 ] || { echo "config blob upload returned $code" >&2; return 1; }
    cfg_dg=$(oci_digest "$d/config.json")
    cfg_size=$(wc -c < "$d/config.json" | tr -d ' ')
    head -c 64 /dev/urandom > "$d/never-uploaded"
    missing=$(oci_digest "$d/never-uploaded")
    jq -n --arg c "$cfg_dg" --argjson cs "$cfg_size" --arg l "$missing" '{
        schemaVersion: 2,
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        config: {mediaType: "application/vnd.oci.image.config.v1+json", digest: $c, size: $cs},
        layers: [{mediaType: "application/vnd.oci.image.layer.v1.tar", digest: $l, size: 64}]}' > "$d/manifest.json"
    code=$(oci_curl -u "bb:$TOKEN" -X PUT -o "$d/put.json" -w '%{http_code}' \
        -H 'Content-Type: application/vnd.oci.image.manifest.v1+json' --data-binary @"$d/manifest.json" \
        "$MEGA2_BASE_URL/v2/$repo/manifests/v1")
    [ "$code" = 400 ] || { echo "manifest PUT returned $code" >&2; return 1; }
    [ "$(jq -r '.errors[0].code' "$d/put.json")" = MANIFEST_BLOB_UNKNOWN ] || { echo "error code is not MANIFEST_BLOB_UNKNOWN" >&2; return 1; }
    code=$(oci_curl -I -H 'Accept: application/vnd.oci.image.manifest.v1+json' -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/manifests/v1")
    [ "$code" = 404 ] || { echo "HEAD of the rejected tag returned $code" >&2; return 1; }
}

# --- BB-21 OCI delete unsupported ---------------------------------------
case_oci_delete_unsupported() {
    local d="$WORK/delete" repo="bb-$RUN_ID/delete" dg layer code accept
    oci_case_begin
    accept='Accept: application/vnd.oci.image.manifest.v1+json'
    dg=$(oci_oras_push "$repo:v1" "$d")
    code=$(oci_curl -H "$accept" -o "$d/manifest.json" -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/manifests/$dg")
    [ "$code" = 200 ] || { echo "manifest GET returned $code" >&2; return 1; }
    layer=$(jq -r '.layers[0].digest' "$d/manifest.json")
    [[ "$layer" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "manifest has no valid layer digest" >&2; return 1; }
    code=$(oci_curl -I -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/blobs/$layer")
    [ "$code" = 200 ] || { echo "layer blob HEAD before DELETE returned $code" >&2; return 1; }
    code=$(oci_curl -u "bb:$TOKEN" -X DELETE -o "$d/del-m.json" -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/manifests/$dg")
    [ "$code" = 405 ] || { echo "manifest DELETE returned $code" >&2; return 1; }
    [ "$(jq -r '.errors[0].code' "$d/del-m.json")" = UNSUPPORTED ] || { echo "manifest DELETE code is not UNSUPPORTED" >&2; return 1; }
    code=$(oci_curl -u "bb:$TOKEN" -X DELETE -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/blobs/$layer")
    [ "$code" = 405 ] || { echo "blob DELETE returned $code" >&2; return 1; }
    code=$(oci_curl -H "$accept" -o /dev/null -w '%{http_code}' "$MEGA2_BASE_URL/v2/$repo/manifests/$dg")
    [ "$code" = 200 ] || { echo "manifest GET after DELETE returned $code" >&2; return 1; }
}

# --- BB-22 OCI catalog not implemented ----------------------------------
case_oci_catalog_not_implemented() {
    local d="$WORK/catalog" code
    oci_case_begin
    mkdir -p "$d"
    code=$(oci_curl -o "$d/catalog.json" -w '%{http_code}' "$MEGA2_BASE_URL/v2/_catalog")
    [ "$code" = 400 ] || { echo "GET /v2/_catalog returned $code" >&2; return 1; }
    [ "$(jq -r '.errors[0].code' "$d/catalog.json")" = NAME_INVALID ] || { echo "_catalog error code is not NAME_INVALID" >&2; return 1; }
}

run_case "OCI ping" case_oci_ping
run_case "OCI oras push and pull" case_oci_oras_push_pull
run_case "OCI reject unauthenticated push" case_oci_reject_unauth_push
run_case "OCI chunked blob upload" case_oci_chunked_upload
run_case "OCI cross-repo blob mount" case_oci_cross_repo_mount
run_case "OCI manifest HEAD and conditional GET" case_oci_manifest_head_304
run_case "OCI blob range" case_oci_blob_range
run_case "OCI tags list pagination" case_oci_tags_pagination
run_case "OCI image index manifest" case_oci_image_index
run_case "OCI reject manifest with unknown blob" case_oci_manifest_unknown_blob
run_case "OCI delete unsupported" case_oci_delete_unsupported
run_case "OCI catalog not implemented" case_oci_catalog_not_implemented

finish
