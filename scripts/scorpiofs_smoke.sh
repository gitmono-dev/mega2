#!/usr/bin/env bash
# Stack-level ScorpioFS <-> mega2 smoke for the compose IT stack (profile `scorpio`).
# Clients: curl + `docker compose exec`. Do NOT use libra as a protocol/API client.
# Registration: docs/refactoring/test-infra.md ("scorpiofs"); flow: docs/development.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/lib/mega2-it.sh"

usage() {
  cat <<'USAGE'
ScorpioFS stack smoke against the compose-hosted mega2 (project mega2-it).

Drives the ScorpioFS HTTP API from the host (127.0.0.1:12725) and inspects the
FUSE workspace inside the `scorpiofs` container (the mount only exists in its
mount namespace). Cases:

  health          GET /health reports status=ok (+ prints scorpio --version)
  dicfuse-root    read-only root lists mega2's initialized tree (project, third-party)
  dicfuse-read    project/.gitkeep content matches the monorepo-init placeholder
  legacy-mount    POST /api/fs/mount -> GET /api/fs/mpoint -> POST /api/fs/unmount
  antares-mount   POST /antares/mounts -> /ready -> ls mountpoint -> DELETE

Bring the stack up first:
  ./scripts/dev-test.sh up-scorpio
  # = docker compose -p mega2-it -f docker-compose.test.yml --profile app --profile scorpio up -d --wait

Environment:
  MEGA2_IT_SCORPIO_URL   ScorpioFS API base on the host (default http://127.0.0.1:12725)
  MEGA2_IT_PROJECT       Compose project name (default mega2-it)
  MEGA2_SMOKE_CASE       Run exactly one case; unmatched -> exit 2
  SCORPIOFS_IT           Set to 1 to fail (instead of SKIP) when the API is not listening
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

command -v curl >/dev/null 2>&1 || {
  echo "curl is required (smoke client; do not use libra)" >&2
  exit 2
}

mega2_it_require_repo_root

SCORPIO_URL="${MEGA2_IT_SCORPIO_URL:-http://127.0.0.1:12725}"
SCORPIO_URL="${SCORPIO_URL%/}"
WORKSPACE_ROOT="/var/lib/scorpiofs/mount"
CASE_FILTER="${MEGA2_SMOKE_CASE:-}"
CASE_HIT=0
PASS_COUNT=0
FAIL_COUNT=0

# The service lives in profile `scorpio` and depends on `mega2` (profile `app`);
# compose only resolves it when both profiles are active.
compose_exec() {
  mega2_it_compose --profile app --profile scorpio exec -T scorpiofs "$@"
}

# `key` extraction for the compact serde_json payloads ScorpioFS emits
# (string values only). Avoids a jq dependency on the host.
json_str() {
  local key="$1"
  sed -n "s/.*\"${key}\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -n 1
}

http_get() {
  curl -sS -f -m 20 "$@"
}

http_json() {
  local method="$1" path="$2" body="${3:-}"
  local -a args=(-sS -m 60 -X "$method" -H 'Content-Type: application/json')
  if [[ -n "$body" ]]; then
    args+=(-d "$body")
  fi
  curl "${args[@]}" "${SCORPIO_URL}${path}"
}

# Retry a container-side listing while dicfuse finishes its lazy root load.
retry() {
  local attempts="$1"
  shift
  local i
  for ((i = 1; i <= attempts; i++)); do
    if "$@"; then
      return 0
    fi
    sleep 1
  done
  return 1
}

run_case() {
  local name="$1"
  shift
  if [[ -n "$CASE_FILTER" && "$name" != "$CASE_FILTER" ]]; then
    return 0
  fi
  CASE_HIT=1
  echo "==> $name"
  if "$@"; then
    echo "PASS: $name"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "FAIL: $name" >&2
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

# --- availability gate -------------------------------------------------------

if ! http_get -o /dev/null "${SCORPIO_URL}/health" 2>/dev/null; then
  if [[ "${SCORPIOFS_IT:-}" == "1" ]]; then
    echo "FAIL: ScorpioFS API not reachable at ${SCORPIO_URL} (SCORPIOFS_IT=1)" >&2
    exit 1
  fi
  echo "SKIP: ScorpioFS API not listening at ${SCORPIO_URL}; run ./scripts/dev-test.sh up-scorpio first"
  exit 0
fi

# --- cases -------------------------------------------------------------------

case_health() {
  local body
  body="$(http_get "${SCORPIO_URL}/health")" || return 1
  echo "health: $body"
  if [[ "$(printf '%s' "$body" | json_str status)" != "ok" ]]; then
    echo "expected status=ok" >&2
    return 1
  fi
  echo "container: $(compose_exec scorpio --version 2>/dev/null || echo 'scorpio --version unavailable')"
}

root_lists_init_tree() {
  local listing
  listing="$(compose_exec ls -1 "$WORKSPACE_ROOT" 2>/dev/null)" || return 1
  printf '%s\n' "$listing" | grep -qx 'project' && printf '%s\n' "$listing" | grep -qx 'third-party'
}

case_dicfuse_root() {
  if ! retry 30 root_lists_init_tree; then
    echo "workspace root did not list mega2's init tree (project, third-party):" >&2
    compose_exec ls -la "$WORKSPACE_ROOT" >&2 || true
    return 1
  fi
  compose_exec ls -1 "$WORKSPACE_ROOT"
}

case_dicfuse_read() {
  local content expected='Placeholder file for /project directory'
  content="$(compose_exec cat "${WORKSPACE_ROOT}/project/.gitkeep")" || {
    echo "cannot read ${WORKSPACE_ROOT}/project/.gitkeep" >&2
    return 1
  }
  echo "project/.gitkeep: $content"
  if [[ "$content" != *"$expected"* ]]; then
    echo "expected content to contain: $expected" >&2
    return 1
  fi
}

case_legacy_mount() {
  local resp request_id status
  resp="$(http_json POST /api/fs/mount '{"path":"project"}')" || return 1
  echo "mount: $resp"
  status="$(printf '%s' "$resp" | json_str status)"
  if [[ "$status" != "Success" ]]; then
    echo "expected status=Success from POST /api/fs/mount" >&2
    return 1
  fi
  request_id="$(printf '%s' "$resp" | json_str request_id)"
  if [[ -n "$request_id" ]]; then
    resp="$(http_json GET "/api/fs/select/${request_id}")" || return 1
    echo "select: $resp"
  fi

  resp="$(http_json GET /api/fs/mpoint)" || return 1
  echo "mpoint: $resp"
  if ! printf '%s' "$resp" | grep -q '"path"[[:space:]]*:[[:space:]]*"project"'; then
    echo "GET /api/fs/mpoint does not list path=project" >&2
    return 1
  fi

  compose_exec ls -1a "${WORKSPACE_ROOT}/project" || {
    echo "mounted workspace ${WORKSPACE_ROOT}/project is not listable" >&2
    return 1
  }

  resp="$(http_json POST /api/fs/unmount '{"path":"project"}')" || return 1
  echo "unmount: $resp"
  if [[ "$(printf '%s' "$resp" | json_str status)" != "Success" ]]; then
    echo "expected status=Success from POST /api/fs/unmount" >&2
    return 1
  fi
}

case_antares_mount() {
  local job_id="scorpio-smoke-$$-$(date +%s)"
  local resp mount_id mountpoint i ready=0
  resp="$(http_json POST /antares/mounts "{\"job_id\":\"${job_id}\",\"path\":\"/project\"}")" || return 1
  echo "antares mount: $resp"
  mount_id="$(printf '%s' "$resp" | json_str mount_id)"
  mountpoint="$(printf '%s' "$resp" | json_str mountpoint)"
  if [[ -z "$mount_id" || -z "$mountpoint" ]]; then
    echo "POST /antares/mounts did not return mount_id/mountpoint" >&2
    return 1
  fi

  for ((i = 1; i <= 60; i++)); do
    resp="$(http_json GET "/antares/mounts/${mount_id}/ready")" || true
    if printf '%s' "$resp" | grep -q '"ready"[[:space:]]*:[[:space:]]*true'; then
      ready=1
      break
    fi
    sleep 1
  done
  echo "ready: $resp"
  if [[ "$ready" -ne 1 ]]; then
    echo "mount ${mount_id} never reported ready=true" >&2
    http_json DELETE "/antares/mounts/${mount_id}" >/dev/null 2>&1 || true
    return 1
  fi

  if ! compose_exec ls -1a "$mountpoint" | grep -qx '.gitkeep'; then
    echo "antares mountpoint ${mountpoint} does not expose project/.gitkeep" >&2
    http_json DELETE "/antares/mounts/${mount_id}" >/dev/null 2>&1 || true
    return 1
  fi

  resp="$(http_json DELETE "/antares/mounts/${mount_id}")" || return 1
  echo "antares delete: $resp"
  if ! printf '%s' "$resp" | grep -q '"state"[[:space:]]*:[[:space:]]*"Unmounted"'; then
    echo "expected state=Unmounted from DELETE /antares/mounts/${mount_id}" >&2
    return 1
  fi
}

run_case health case_health
run_case dicfuse-root case_dicfuse_root
run_case dicfuse-read case_dicfuse_read
run_case legacy-mount case_legacy_mount
run_case antares-mount case_antares_mount

if [[ -n "$CASE_FILTER" && "$CASE_HIT" -eq 0 ]]; then
  echo "no case named '${CASE_FILTER}'" >&2
  exit 2
fi

echo "scorpiofs smoke: ${PASS_COUNT} passed, ${FAIL_COUNT} failed"
[[ "$FAIL_COUNT" -eq 0 ]]
