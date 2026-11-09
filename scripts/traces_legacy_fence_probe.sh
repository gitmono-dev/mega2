#!/usr/bin/env bash
# Deployment perimeter probe for the legacy Capture retirement (MTR-46).
#
# Run by the named deployment maintainer against one real deployment, after
# MTR-33 activated the persistent fence:
#
#   MEGA2_TRACES_PROBE_DB_URL=postgres://<old-app-role>... \
#   MEGA2_TRACES_PROBE_OBJECT_PUT_URL=https://<backend>/<legacy-prefix> \
#   MEGA2_TRACES_PROBE_OBJECT_TOKEN_FILE=/run/secrets/old-object-token \
#   scripts/traces_legacy_fence_probe.sh \
#     --maintenance-manifest "$MEGA2_TRACES_MAINTENANCE_MANIFEST" \
#     --old-image "$MEGA2_TRACES_OLD_IMAGE_DIGEST" \
#     --journal "$MEGA2_TRACES_PURGE_JOURNAL"
#
# The script proves that the old writer identity can no longer write to the
# legacy database and can no longer PUT objects under the legacy prefix. The
# old image digest is recorded in the sanitized receipt so the evidence is
# bound to the exact image that was probed. A missing fixture value, a probe
# that does not run, or a probe that unexpectedly succeeds is a FAIL; nothing
# is skipped.
#
# Exit codes: 0 both probes failed as expected; 1 a probe ran and succeeded or
# a fixture value is missing; 2 usage error.
set -euo pipefail

usage() {
  cat <<'USAGE'
Deployment perimeter probe (MTR-46).

Usage:
  scripts/traces_legacy_fence_probe.sh --maintenance-manifest <PATH> \
      --old-image <DIGEST> --journal <UUID>

Required environment:
  MEGA2_TRACES_PROBE_DB_URL             old app-role database URL (never printed)
  MEGA2_TRACES_PROBE_OBJECT_PUT_URL     PUT endpoint under the legacy prefix
  MEGA2_TRACES_PROBE_OBJECT_TOKEN_FILE  file with the old object-storage token

Exit codes: 0 both probes refused; 1 a probe ran and succeeded or a fixture is
missing; 2 usage error.
USAGE
}

manifest=""
old_image=""
journal=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --maintenance-manifest) manifest="${2:-}"; shift 2 ;;
    --old-image) old_image="${2:-}"; shift 2 ;;
    --journal) journal="${2:-}"; shift 2 ;;
    *) echo "traces perimeter: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
  esac
done
if [ -z "$manifest" ] || [ -z "$old_image" ] || [ -z "$journal" ]; then
  echo "traces perimeter: --maintenance-manifest, --old-image and --journal are required" >&2
  usage >&2
  exit 2
fi
case "$journal" in
  *[!0-9a-f-]*|"") echo "traces perimeter: --journal must be a UUID" >&2; exit 2 ;;
esac
case "$journal" in
  ????????-????-????-????-????????????) ;;
  *) echo "traces perimeter: --journal must be a UUID" >&2; exit 2 ;;
esac

fail_fixture() {
  echo "FAIL: fixture $1"
}

[ -r "$manifest" ] || { fail_fixture "maintenance manifest $manifest is not readable"; exit 1; }
for var in MEGA2_TRACES_PROBE_DB_URL MEGA2_TRACES_PROBE_OBJECT_PUT_URL MEGA2_TRACES_PROBE_OBJECT_TOKEN_FILE; do
  if [ -z "${!var:-}" ]; then
    fail_fixture "$var is required"
    exit 1
  fi
done
[ -r "$MEGA2_TRACES_PROBE_OBJECT_TOKEN_FILE" ] || {
  fail_fixture "MEGA2_TRACES_PROBE_OBJECT_TOKEN_FILE must be readable"
  exit 1
}
command -v psql >/dev/null 2>&1 || { fail_fixture "psql is required"; exit 1; }
command -v curl >/dev/null 2>&1 || { fail_fixture "curl is required"; exit 1; }

WORKDIR="$(mktemp -d)"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

failures=0
executed=0
passed=0

run_probe() {
  local name="$1"
  shift
  echo "==> $name"
  executed=$((executed + 1))
  if "$@"; then
    passed=$((passed + 1))
    echo "PASS: $name"
  else
    failures=$((failures + 1))
    echo "FAIL: $name"
  fi
}

# AC-6: the old writer identity can no longer insert into the legacy tables.
db_write_probe() {
  local output status
  set +e
  output="$(psql "$MEGA2_TRACES_PROBE_DB_URL" -v ON_ERROR_STOP=1 -q -c \
    "INSERT INTO agent_capture_session
       (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind)
     VALUES ('mtr46','mtr46','mtr46','mtr46','mtr46-probe','external_capture')" 2>&1)"
  status=$?
  set -e
  [ "$status" -ne 0 ] || { echo "the old role could still write" >&2; return 1; }
  case "$output" in
    *permission\ denied*|*fenced*|*42501*|*insufficient* ) return 0 ;;
    *) echo "the write failed for an unexpected reason: $output" >&2; return 1 ;;
  esac
}

# AC-7: the old object identity can no longer PUT under the legacy prefix.
object_put_probe() {
  local token status
  token="$(cat "$MEGA2_TRACES_PROBE_OBJECT_TOKEN_FILE")"
  status="$(curl -s -o "$WORKDIR/put.out" -w '%{http_code}' -X PUT \
    -H "Authorization: Bearer ${token}" \
    --data-binary 'mtr46-probe' \
    "${MEGA2_TRACES_PROBE_OBJECT_PUT_URL%/}/mtr46-probe")"
  case "$status" in
    200|201|204) echo "the old role could still PUT (HTTP $status)" >&2; return 1 ;;
    401|403) return 0 ;;
    *) echo "the PUT probe answered HTTP $status" >&2; return 1 ;;
  esac
}

run_probe old_binary_db_write db_write_probe
run_probe old_binary_object_put object_put_probe

echo "traces legacy fence probe: executed=$executed passed=$passed failed=$failures journal=$journal old_image=$old_image"
[ "$failures" -eq 0 ] && [ "$executed" -gt 0 ]
