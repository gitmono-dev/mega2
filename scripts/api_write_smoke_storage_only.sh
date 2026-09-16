#!/usr/bin/env bash
# Trunk / storage-only product API write → Git visibility black-box (plan-20260904 AW-04).
# Clients: curl + git only. Do NOT use libra as a protocol/API test client.
# Inheritable into compose git-smoke; plan-20260906 may absorb cases later (DEP-AW-OUT-01).
set -euo pipefail

usage() {
  cat <<'USAGE'
API write → Git clone/pull smoke for storage-only / trunk (compose git-smoke).

Clients: curl, git. Do not use libra as a client.

Required environment:
  MEGA2_API_BASE           e.g. http://mega2:8000
  MEGA2_HTTP_REPO_URL      Smart HTTP URL (credentialed or with MEGA2_IT_SEED_TOKEN)
  MEGA2_IT_SEED_TOKEN      Push token (Bearer / Basic) when URL has no userinfo

Optional:
  MEGA2_SMOKE_CASE         Exact case name; unmatched → exit 2
  MEGA2_GIT_SMOKE_WORKDIR Existing directory for temporary clones
  MEGA2_GIT_SMOKE_KEEP_WORKDIR  Set to 1 to keep temporary clones

Compose example (after mega2-trunk up + service init):
  TOKEN='mega2-storage-only-local-dev-token-0001'
  docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
    exec -T \
    -e MEGA2_HTTP_REPO_URL="http://x:${TOKEN}@mega2:8000/" \
    -e MEGA2_API_BASE=http://mega2:8000 \
    -e MEGA2_IT_SEED_TOKEN="${TOKEN}" \
    git-smoke bash /repo/scripts/api_write_smoke_storage_only.sh
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

command -v curl >/dev/null 2>&1 || {
  echo "curl is required (API smoke client; do not use libra)" >&2
  exit 2
}
command -v git >/dev/null 2>&1 || {
  echo "git is required (protocol smoke client; do not use libra)" >&2
  exit 2
}

ROOT_DIR="${MEGA2_GIT_SMOKE_WORKDIR:-$(mktemp -d)}"
if [[ ! -d "$ROOT_DIR" ]]; then
  mkdir -p "$ROOT_DIR"
fi

cleanup() {
  if [[ -z "${MEGA2_GIT_SMOKE_WORKDIR:-}" && "${MEGA2_GIT_SMOKE_KEEP_WORKDIR:-}" != "1" ]]; then
    rm -rf "$ROOT_DIR"
  else
    echo "smoke workdir kept at: $ROOT_DIR"
  fi
}
trap cleanup EXIT

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
CASE_FILTER="${MEGA2_SMOKE_CASE:-}"
CASE_HIT=0

# Shared paths / file names for the create → clone → save → pull chain.
PROJECT_PATH="/project"
CREATE_NAME="aw04-api-created.txt"
CREATE_CONTENT="aw04 create-entry body $$"
SAVE_CONTENT="aw04 edit-save body $$"

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

require_api_base() {
  if [[ -z "${MEGA2_API_BASE:-}" ]]; then
    echo "MEGA2_API_BASE is required" >&2
    return 1
  fi
}

require_http_url() {
  if [[ -z "${MEGA2_HTTP_REPO_URL:-}" ]]; then
    echo "MEGA2_HTTP_REPO_URL is required" >&2
    return 1
  fi
}

require_token() {
  if [[ -z "${MEGA2_IT_SEED_TOKEN:-}" ]]; then
    echo "MEGA2_IT_SEED_TOKEN is required for API write auth" >&2
    return 1
  fi
}

project_http_url() {
  local url="${MEGA2_HTTP_REPO_URL%/}"
  local token="${MEGA2_IT_SEED_TOKEN:-}"
  # Already has userinfo.
  if [[ "$url" =~ ^https?://[^/@]+:[^/@]+@ ]]; then
    :
  elif [[ -n "$token" ]]; then
    case "$url" in
      http://*) url="http://x:${token}@${url#http://}" ;;
      https://*) url="https://x:${token}@${url#https://}" ;;
    esac
  fi
  if [[ "$url" == */project ]]; then
    printf '%s/\n' "$url"
  else
    # Strip trailing path noise; append /project for B0.
    printf '%s/project/\n' "$url"
  fi
}

git_case() {
  git -c advice.detachedHead=false "$@"
}

api_auth_header() {
  printf 'Authorization: Bearer %s\n' "${MEGA2_IT_SEED_TOKEN}"
}

# Ensure /project has a path tip so land_api_tip_push (B0) can succeed.
ensure_project_tip() {
  require_http_url || return 1
  require_token || return 1
  local url dest
  url="$(project_http_url)"
  dest="$ROOT_DIR/aw04-seed-project"
  rm -rf "$dest"
  GIT_LFS_SKIP_SMUDGE=1 git_case clone "$url" "$dest" >/dev/null || return 1
  git -C "$dest" config user.name "Mega2 API Smoke" || return 1
  git -C "$dest" config user.email "mega2-api-smoke@example.invalid" || return 1
  # If tip already has commits beyond empty clone, still ensure a marker file once.
  if [[ ! -f "$dest/aw04-seed.txt" ]]; then
    printf 'aw04 seed tip\n' >"$dest/aw04-seed.txt"
    git -C "$dest" add aw04-seed.txt || return 1
    git -C "$dest" commit -m "aw04 seed /project tip" >/dev/null || return 1
    git_case -C "$dest" -c pack.window=0 -c pack.depth=0 push --no-thin origin HEAD:refs/heads/main \
      >/dev/null || return 1
  fi
}

case_api_create_then_clone() {
  require_api_base || return 1
  require_http_url || return 1
  require_token || return 1
  ensure_project_tip || return 1

  local resp code body dest
  resp="$(mktemp)"
  set +e
  code="$(
    curl -sS -o "$resp" -w '%{http_code}' \
      -X POST "${MEGA2_API_BASE%/}/api/v1/create-entry" \
      -H "$(api_auth_header)" \
      -H 'Content-Type: application/json' \
      -d "{\"is_directory\":false,\"name\":\"${CREATE_NAME}\",\"path\":\"${PROJECT_PATH}\",\"content\":\"${CREATE_CONTENT}\\n\",\"skip_build\":true}"
  )"
  set -e
  body="$(cat "$resp")"
  rm -f "$resp"
  if [[ "$code" != "200" ]]; then
    echo "FAIL: create-entry HTTP $code body=$body" >&2
    return 1
  fi
  if ! printf '%s' "$body" | rg -q '"req_result"[[:space:]]*:[[:space:]]*true'; then
    echo "FAIL: create-entry CommonResult.req_result not true: $body" >&2
    return 1
  fi
  if printf '%s' "$body" | rg -q '"cl_link"[[:space:]]*:[[:space:]]*"[^"]+"'; then
    echo "FAIL: trunk create must not return a CL link: $body" >&2
    return 1
  fi

  dest="$ROOT_DIR/aw04-clone-after-create"
  rm -rf "$dest"
  GIT_LFS_SKIP_SMUDGE=1 git_case clone "$(project_http_url)" "$dest" >/dev/null || return 1
  if [[ ! -f "$dest/$CREATE_NAME" ]]; then
    echo "FAIL: git clone missing $CREATE_NAME" >&2
    return 1
  fi
  if ! rg -Fq "$CREATE_CONTENT" "$dest/$CREATE_NAME"; then
    echo "FAIL: cloned file content mismatch:" >&2
    cat "$dest/$CREATE_NAME" >&2
    return 1
  fi
}

case_api_save_then_pull() {
  require_api_base || return 1
  require_http_url || return 1
  require_token || return 1
  ensure_project_tip || return 1

  # Ensure the file exists (create if prior case was filtered out).
  local dest resp code body
  dest="$ROOT_DIR/aw04-pull-clone"
  rm -rf "$dest"
  GIT_LFS_SKIP_SMUDGE=1 git_case clone "$(project_http_url)" "$dest" >/dev/null || return 1
  if [[ ! -f "$dest/$CREATE_NAME" ]]; then
    resp="$(mktemp)"
    set +e
    code="$(
      curl -sS -o "$resp" -w '%{http_code}' \
        -X POST "${MEGA2_API_BASE%/}/api/v1/create-entry" \
        -H "$(api_auth_header)" \
        -H 'Content-Type: application/json' \
        -d "{\"is_directory\":false,\"name\":\"${CREATE_NAME}\",\"path\":\"${PROJECT_PATH}\",\"content\":\"${CREATE_CONTENT}\\n\",\"skip_build\":true}"
    )"
    set -e
    body="$(cat "$resp")"
    rm -f "$resp"
    if [[ "$code" != "200" ]]; then
      echo "FAIL: prerequisite create-entry HTTP $code body=$body" >&2
      return 1
    fi
    git_case -C "$dest" pull --ff-only origin main >/dev/null || return 1
  fi

  resp="$(mktemp)"
  set +e
  code="$(
    curl -sS -o "$resp" -w '%{http_code}' \
      -X POST "${MEGA2_API_BASE%/}/api/v1/edit/save" \
      -H "$(api_auth_header)" \
      -H 'Content-Type: application/json' \
      -d "{\"path\":\"${PROJECT_PATH}/${CREATE_NAME}\",\"content\":\"${SAVE_CONTENT}\\n\",\"commit_message\":\"aw04 edit/save\",\"skip_build\":true}"
  )"
  set -e
  body="$(cat "$resp")"
  rm -f "$resp"
  if [[ "$code" != "200" ]]; then
    echo "FAIL: edit/save HTTP $code body=$body" >&2
    return 1
  fi
  if ! printf '%s' "$body" | rg -q '"req_result"[[:space:]]*:[[:space:]]*true'; then
    echo "FAIL: edit/save CommonResult.req_result not true: $body" >&2
    return 1
  fi

  git_case -C "$dest" pull --ff-only origin main >/dev/null || return 1
  if ! rg -Fq "$SAVE_CONTENT" "$dest/$CREATE_NAME"; then
    echo "FAIL: after git pull, file content mismatch:" >&2
    cat "$dest/$CREATE_NAME" >&2
    return 1
  fi
}

case_api_write_rejects_unauthenticated() {
  require_api_base || return 1
  local resp code body tip_probe
  resp="$(mktemp)"
  set +e
  code="$(
    curl -sS -o "$resp" -w '%{http_code}' \
      -X POST "${MEGA2_API_BASE%/}/api/v1/create-entry" \
      -H 'Content-Type: application/json' \
      -d "{\"is_directory\":false,\"name\":\"aw04-unauth.txt\",\"path\":\"${PROJECT_PATH}\",\"content\":\"should-not-land\\n\",\"skip_build\":true}"
  )"
  set -e
  body="$(cat "$resp")"
  rm -f "$resp"
  if [[ "$code" != "401" ]]; then
    echo "FAIL: unauthenticated create-entry must be HTTP 401, got $code body=$body" >&2
    return 1
  fi
  # Tip must not advertise the unauth file via anonymous clone of /project.
  tip_probe="$ROOT_DIR/aw04-unauth-probe"
  rm -rf "$tip_probe"
  # Clone may require credentials when anonymous_access=false; use seeded URL.
  require_http_url || return 1
  GIT_LFS_SKIP_SMUDGE=1 git_case clone "$(project_http_url)" "$tip_probe" >/dev/null || return 1
  if [[ -f "$tip_probe/aw04-unauth.txt" ]]; then
    echo "FAIL: unauthenticated write must not leave aw04-unauth.txt on tip" >&2
    return 1
  fi
}

run_case "API create-entry then git clone sees file" case_api_create_then_clone
run_case "API edit/save then git pull sees update" case_api_save_then_pull
run_case "API write rejects unauthenticated" case_api_write_rejects_unauthenticated

if [[ -n "$CASE_FILTER" && "$CASE_HIT" -eq 0 ]]; then
  echo "FAIL: MEGA2_SMOKE_CASE='$CASE_FILTER' matched no registered case" >&2
  echo "api write smoke storage_only summary: 0 passed, 1 failed (${SKIP_COUNT} skipped)"
  exit 2
fi

echo "api write smoke storage_only summary: $PASS_COUNT passed, $FAIL_COUNT failed (${SKIP_COUNT} skipped)"
if [[ "$FAIL_COUNT" -gt 0 ]]; then
  exit 1
fi
exit 0
