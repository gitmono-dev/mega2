#!/usr/bin/env bash
# Host-side presigned URL round trip against the storage-only trunk stack
# (docs/deploy-trunk.md §8; plan-20261001 FIX-BB-03): artifacts batch ->
# presigned PUT -> commit -> download following the 302 to the presigned GET
# -> sha256 compare. Exits 0 only when the upload URL and the download
# redirect are signed for MEGA2_PRESIGN_ORIGIN (default the host-published
# RustFS port http://127.0.0.1:29000), the PUT is 2xx, the commit status is
# `ok` and the downloaded bytes match, all within MEGA2_PRESIGN_CHECK_BUDGET
# seconds (default 60); 2 when a required tool or the token is missing.
# The push token comes from MEGA2_IT_SEED_TOKEN and is never printed or put on
# a command line; signed URLs are printed without their query.
set -euo pipefail

BASE_URL="${MEGA2_BASE_URL:-http://127.0.0.1:9000}"
PRESIGN_ORIGIN="${MEGA2_PRESIGN_ORIGIN:-http://127.0.0.1:29000}"
TOKEN="${MEGA2_IT_SEED_TOKEN:-}"
if [ -z "$TOKEN" ]; then
  echo "set MEGA2_IT_SEED_TOKEN to the trunk push token" >&2
  exit 2
fi

for tool in curl jq od tr head cut wc mktemp date chmod rm; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 2; }
done
if command -v sha256sum >/dev/null; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  echo "missing tool: sha256sum or shasum" >&2
  exit 2
fi

RUN_ID="$(date -u +%Y%m%dt%H%M%S)-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
REPO="presign-host-check-$RUN_ID"
NAMESPACE="presign-host-$RUN_ID"
OBJECT_PATH="presign/check-$RUN_ID.bin"
API="$BASE_URL/api/v1/repos/$REPO/artifacts"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
chmod 700 "$work"
(umask 077; printf 'Authorization: Bearer %s\n' "$TOKEN" > "$work/auth.h")

strip_query() { printf '%s\n' "${1%%\?*}"; }
origin_of() {
  local rest="${1#*://}"
  printf '%s://%s\n' "${1%%://*}" "${rest%%[/?#]*}"
}
# is_presigned <url>: the URL carries SigV4 query authentication. The stack's
# bucket allows anonymous downloads, so a plain URL would also be served.
is_presigned() {
  local url="${1%%#*}" query
  query="${url#*\?}"
  [ "$query" != "$url" ] || return 1
  case "&$query&" in *"&X-Amz-Algorithm=AWS4-HMAC-SHA256&"*) ;; *) return 1 ;; esac
  case "&$query&" in *"&X-Amz-Signature="[!\&]*) return 0 ;; *) return 1 ;; esac
}
DEADLINE=$((SECONDS + ${MEGA2_PRESIGN_CHECK_BUDGET:-60}))
# http: curl without redirects, bounded by the time left in the round trip.
http() {
  local left=$((DEADLINE - SECONDS))
  if [ "$left" -le 0 ]; then
    echo "time budget exhausted" >&2
    return 1
  fi
  curl -sS --proto '=http,https' --max-redirs 0 --connect-timeout 5 --max-time "$left" "$@"
}

hex="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
variant="$(printf '%x' $(( (0x${hex:16:1} & 0x3) | 0x8 )))"
oid="${hex:0:8}-${hex:8:4}-4${hex:13:3}-${variant}${hex:17:3}-${hex:20:12}"

head -c 65536 /dev/urandom > "$work/object.bin"
size="$(wc -c < "$work/object.bin" | tr -d ' ')"
want_sha="$(sha256_of "$work/object.bin")"

jq -n --arg ns "$NAMESPACE" --arg path "$OBJECT_PATH" --arg oid "$oid" --argjson size "$size" \
  '{namespace: $ns, object_type: "snapshot", intent: "upload",
    objects: [{path: $path, oid: $oid, size: $size}]}' > "$work/batch.json"
batch_status="$(http -o "$work/batch.out" -w '%{http_code}' -H @"$work/auth.h" \
  -H 'Content-Type: application/json' --data @"$work/batch.json" "$API/batch")"
echo "batch-status: $batch_status"
test "$batch_status" = 200

href="$(jq -r '.objects[0].actions.upload.href // empty' "$work/batch.out")"
if [ -z "$href" ]; then
  echo "no presigned upload action in the batch response (backend without presign?)"
  exit 1
fi
content_type="$(jq -r '.objects[0].actions.upload.header["Content-Type"] // "application/octet-stream"' "$work/batch.out")"
echo "presign-url: $(strip_query "$href")"
if [ "$(origin_of "$href")" != "$PRESIGN_ORIGIN" ] || ! is_presigned "$href"; then
  echo "upload URL is not presigned for $PRESIGN_ORIGIN"
  exit 1
fi

put_status="$(http -o /dev/null -w '%{http_code}' -X PUT -H "Content-Type: $content_type" \
  --data-binary @"$work/object.bin" "$href")"
echo "presign-put: $put_status"
case "$put_status" in 2??) ;; *) exit 1 ;; esac

jq -n --arg ns "$NAMESPACE" --arg path "$OBJECT_PATH" --arg oid "$oid" --argjson size "$size" \
  '{namespace: $ns, object_type: "snapshot", files: [{path: $path, oid: $oid, size: $size}]}' > "$work/commit.json"
commit_http="$(http -o "$work/commit.out" -w '%{http_code}' -H @"$work/auth.h" \
  -H 'Content-Type: application/json' --data @"$work/commit.json" "$API/commit")"
commit_status="$(jq -r '.status // empty' "$work/commit.out" 2>/dev/null || true)"
echo "commit-status: ${commit_status:-none} (http $commit_http)"
test "$commit_http" = 200
test "$commit_status" = ok

# Follow the 302 by hand: check the Location origin before requesting it.
http -o /dev/null -w '%{http_code} %{redirect_url}\n' "$API/objects/$oid" > "$work/redirect.meta"
read -r redirect_code location < "$work/redirect.meta" || true
echo "download-redirect: ${redirect_code:-none} url=$(strip_query "${location:-}")"
test "${redirect_code:-}" = 302
if [ "$(origin_of "${location:-}")" != "$PRESIGN_ORIGIN" ] || ! is_presigned "${location:-}"; then
  echo "download redirect is not presigned for $PRESIGN_ORIGIN"
  exit 1
fi
download_code="$(http -o "$work/download.bin" -w '%{http_code}' "$location")"
echo "download: $download_code"
test "$download_code" = 200

got_sha="$(sha256_of "$work/download.bin")"
if [ "$got_sha" != "$want_sha" ]; then
  echo "sha256 mismatch: want $want_sha got $got_sha"
  exit 1
fi
echo "sha256 match"
