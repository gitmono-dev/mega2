#!/usr/bin/env bash
# Trunk / storage-only product API write → Git visibility black-box (plan-20260904 AW-04).
# plan-20260917 LB-05 adds the directory-change (delete-entry / move-entry) and monorepo
# tag cases (curl against the product API, git clone/pull for visibility).
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

Registered cases (exact names for MEGA2_SMOKE_CASE):
  API create-entry then git clone sees file      (plan-20260904 AW-04)
  API edit/save then git pull sees update        (plan-20260904 AW-04)
  API write rejects unauthenticated              (plan-20260904 AW-04)
  delete-entry-git-visible                       (plan-20260917 LB-05)
  move-entry-git-visible                         (plan-20260917 LB-05)
  tags-list-create-delete                        (plan-20260917 LB-05)
  delete-entry-unauth-401                        (plan-20260917 LB-05)

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

# ---------------------------------------------------------------------------
# plan-20260917 LB-05: directory-change and tag cases.
# ---------------------------------------------------------------------------

# Product API call helper: METHOD route [json] [noauth]. Sets API_CODE (HTTP
# status) and API_BODY (response body) — globals on purpose: a `$(...)` capture
# would run the helper in a subshell and lose the body. For product API calls the
# token travels only in the Authorization header; the Git transport keeps using the
# credentialed URL from project_http_url (as the AW-04 cases do). The script never
# prints the token.
API_CODE=""
API_BODY=""
api_call() {
  local method="$1" route="$2" json="${3:-}" auth="${4:-auth}" resp code
  local -a args=(-sS -o "" -w '%{http_code}' -X "$method" "${MEGA2_API_BASE%/}/api/v1/${route}")
  resp="$(mktemp)"
  args[2]="$resp"
  if [[ "$auth" != "noauth" ]]; then
    args+=(-H "$(api_auth_header)")
  fi
  if [[ -n "$json" ]]; then
    args+=(-H 'Content-Type: application/json' -d "$json")
  fi
  set +e
  code="$(curl "${args[@]}")"
  set -e
  API_BODY="$(cat "$resp")"
  rm -f "$resp"
  API_CODE="$code"
}

# body_matches <ERE>: bash regex over API_BODY (no pipe, so no SIGPIPE under
# pipefail on large bodies).
body_matches() {
  [[ "$API_BODY" =~ $1 ]]
}

body_req_result_true() {
  body_matches '"req_result"[[:space:]]*:[[:space:]]*true'
}

body_has_cl_link() {
  body_matches '"cl_link"[[:space:]]*:[[:space:]]*"[^"]+"'
}

body_names_entry() {
  body_matches "\"name\"[[:space:]]*:[[:space:]]*\"${1}\""
}

# tree_lists <path> <name>: 0 = listed, 1 = not listed, 2 = GET /tree failed.
tree_lists() {
  local code
  api_call GET "tree?path=${1}" "" noauth; code="$API_CODE"
  if [[ "$code" != "200" ]]; then
    echo "GET /tree?path=${1} HTTP $code body=$API_BODY" >&2
    return 2
  fi
  if body_names_entry "$2"; then
    return 0
  fi
  return 1
}

# expect_listed / expect_not_listed <path> <name> <when>: keep the rc=2 (GET
# failed) outcome distinct from a genuine listed / not-listed result.
expect_listed() {
  local rc
  set +e
  tree_lists "$1" "$2"
  rc=$?
  set -e
  case "$rc" in
    0) return 0 ;;
    1) echo "FAIL: GET /tree does not list $2 $3" >&2; return 1 ;;
    *) echo "FAIL: GET /tree failed $3" >&2; return 1 ;;
  esac
}

expect_not_listed() {
  local rc
  set +e
  tree_lists "$1" "$2"
  rc=$?
  set -e
  case "$rc" in
    1) return 0 ;;
    0) echo "FAIL: GET /tree still lists $2 $3" >&2; return 1 ;;
    *) echo "FAIL: GET /tree failed $3" >&2; return 1 ;;
  esac
}

# Names are unique per run: PID alone can repeat after a container restart.
LB05_RUN_ID="$$-$(date +%s)"

# Create a directory under /project with one file inside, through create-entry.
create_dir_with_file() {
  local dir="$1" code
  api_call POST create-entry "{\"is_directory\":true,\"name\":\"${dir}\",\"path\":\"${PROJECT_PATH}\",\"skip_build\":true}"; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_req_result_true; then
    echo "FAIL: create-entry directory ${dir} HTTP $code body=$API_BODY" >&2
    return 1
  fi
  api_call POST create-entry "{\"is_directory\":false,\"name\":\"inner.txt\",\"path\":\"${PROJECT_PATH}/${dir}\",\"content\":\"lb05 ${dir}\\n\",\"skip_build\":true}"; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_req_result_true; then
    echo "FAIL: create-entry file in ${dir} HTTP $code body=$API_BODY" >&2
    return 1
  fi
}

fresh_project_clone() {
  local dest="$1"
  rm -rf "$dest"
  GIT_LFS_SKIP_SMUDGE=1 git_case clone "$(project_http_url)" "$dest" >/dev/null
}

case_delete_entry_git_visible() {
  require_api_base || return 1
  require_http_url || return 1
  require_token || return 1
  ensure_project_tip || return 1
  local dir="lb05-del-${LB05_RUN_ID}" dest code
  create_dir_with_file "$dir" || return 1
  dest="$ROOT_DIR/lb05-delete-clone"
  fresh_project_clone "$dest" || return 1
  if [[ ! -f "$dest/$dir/inner.txt" ]]; then
    echo "FAIL: git clone before delete-entry lacks $dir/inner.txt" >&2
    return 1
  fi
  expect_listed "$PROJECT_PATH" "$dir" "before delete-entry" || return 1
  api_call POST delete-entry "{\"path\":\"${PROJECT_PATH}\",\"name\":\"${dir}\",\"skip_build\":true}"; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_req_result_true; then
    echo "FAIL: delete-entry HTTP $code body=$API_BODY" >&2
    return 1
  fi
  if body_has_cl_link; then
    echo "FAIL: trunk delete-entry must not return a CL link: $API_BODY" >&2
    return 1
  fi
  git_case -C "$dest" pull --ff-only origin main >/dev/null || return 1
  if [[ -e "$dest/$dir" ]]; then
    echo "FAIL: after git pull the deleted directory $dir is still in the work tree" >&2
    return 1
  fi
  expect_not_listed "$PROJECT_PATH" "$dir" "after delete-entry" || return 1
}

case_move_entry_git_visible() {
  require_api_base || return 1
  require_http_url || return 1
  require_token || return 1
  ensure_project_tip || return 1
  local src="lb05-mv-src-${LB05_RUN_ID}" dst="lb05-mv-dst-${LB05_RUN_ID}" dest code
  create_dir_with_file "$src" || return 1
  dest="$ROOT_DIR/lb05-move-clone"
  fresh_project_clone "$dest" || return 1
  if [[ ! -f "$dest/$src/inner.txt" ]]; then
    echo "FAIL: git clone before move-entry lacks $src/inner.txt" >&2
    return 1
  fi
  expect_listed "$PROJECT_PATH" "$src" "before move-entry" || return 1
  api_call POST move-entry "{\"from_path\":\"${PROJECT_PATH}\",\"from_name\":\"${src}\",\"to_path\":\"${PROJECT_PATH}\",\"to_name\":\"${dst}\",\"skip_build\":true}"; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_req_result_true; then
    echo "FAIL: move-entry HTTP $code body=$API_BODY" >&2
    return 1
  fi
  if ! body_matches "\"to_path\"[[:space:]]*:[[:space:]]*\"${PROJECT_PATH}/${dst}\""; then
    echo "FAIL: move-entry receipt to_path mismatch: $API_BODY" >&2
    return 1
  fi
  if body_has_cl_link; then
    echo "FAIL: trunk move-entry must not return a CL link: $API_BODY" >&2
    return 1
  fi
  git_case -C "$dest" pull --ff-only origin main >/dev/null || return 1
  if [[ ! -f "$dest/$dst/inner.txt" ]]; then
    echo "FAIL: after git pull the moved directory $dst/inner.txt is missing" >&2
    return 1
  fi
  if [[ -e "$dest/$src" ]]; then
    echo "FAIL: after git pull the source directory $src still exists" >&2
    return 1
  fi
  expect_listed "$PROJECT_PATH" "$dst" "after move-entry" || return 1
  expect_not_listed "$PROJECT_PATH" "$src" "after move-entry" || return 1
}

case_tags_list_create_delete() {
  require_api_base || return 1
  require_token || return 1
  local name="lb05-tag-${LB05_RUN_ID}" code
  local list_route='tags/list?page=1&per_page=200&path=/'
  api_call GET "$list_route" "" noauth; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_req_result_true; then
    echo "FAIL: GET /tags/list (anonymous) HTTP $code body=$API_BODY" >&2
    return 1
  fi
  api_call POST tags/list '{"pagination":{"page":1,"per_page":200},"additional":"/"}' noauth; code="$API_CODE"
  if [[ "$code" != "405" ]]; then
    echo "FAIL: POST /tags/list must 405, got HTTP $code body=$API_BODY" >&2
    return 1
  fi
  api_call POST tags "{\"name\":\"${name}\",\"message\":\"lb05 smoke tag\",\"tagger_name\":\"lb05\",\"tagger_email\":\"lb05@example.invalid\"}"; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_req_result_true; then
    echo "FAIL: POST /tags (token) HTTP $code body=$API_BODY" >&2
    return 1
  fi
  if ! body_names_entry "$name"; then
    echo "FAIL: create-tag response lacks the tag name: $API_BODY" >&2
    return 1
  fi
  api_call GET "$list_route" "" noauth; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_names_entry "$name"; then
    echo "FAIL: GET /tags/list after create does not list ${name} (HTTP $code): $API_BODY" >&2
    return 1
  fi
  api_call GET "tags/${name}" "" noauth; code="$API_CODE"
  if [[ "$code" != "200" ]]; then
    echo "FAIL: GET /tags/${name} HTTP $code body=$API_BODY" >&2
    return 1
  fi
  api_call DELETE "tags/${name}"; code="$API_CODE"
  if [[ "$code" != "200" ]] || ! body_matches "\"deleted_tag\"[[:space:]]*:[[:space:]]*\"${name}\""; then
    echo "FAIL: DELETE /tags/${name} (token) HTTP $code body=$API_BODY" >&2
    return 1
  fi
  api_call GET "$list_route" "" noauth; code="$API_CODE"
  if [[ "$code" != "200" ]] || body_names_entry "$name"; then
    echo "FAIL: GET /tags/list after delete still lists ${name} (HTTP $code): $API_BODY" >&2
    return 1
  fi
  api_call GET "tags/${name}" "" noauth; code="$API_CODE"
  if [[ "$code" != "404" ]]; then
    echo "FAIL: GET /tags/${name} after delete must be 404, got $code body=$API_BODY" >&2
    return 1
  fi
}

case_delete_entry_unauth_401() {
  require_api_base || return 1
  require_http_url || return 1
  require_token || return 1
  ensure_project_tip || return 1
  local dir="lb05-keep-${LB05_RUN_ID}" dest code
  create_dir_with_file "$dir" || return 1
  api_call POST delete-entry "{\"path\":\"${PROJECT_PATH}\",\"name\":\"${dir}\",\"skip_build\":true}" noauth; code="$API_CODE"
  if [[ "$code" != "401" ]]; then
    echo "FAIL: unauthenticated delete-entry must be HTTP 401, got $code body=$API_BODY" >&2
    return 1
  fi
  if [[ "$API_BODY" == *"${MEGA2_IT_SEED_TOKEN}"* ]]; then
    echo "FAIL: 401 body must not echo the token" >&2
    return 1
  fi
  dest="$ROOT_DIR/lb05-unauth-clone"
  fresh_project_clone "$dest" || return 1
  if [[ ! -f "$dest/$dir/inner.txt" ]]; then
    echo "FAIL: unauthenticated delete-entry must leave $dir/inner.txt on the tip" >&2
    return 1
  fi
  expect_listed "$PROJECT_PATH" "$dir" "after the rejected delete" || return 1
}

run_case "API create-entry then git clone sees file" case_api_create_then_clone
run_case "API edit/save then git pull sees update" case_api_save_then_pull
run_case "API write rejects unauthenticated" case_api_write_rejects_unauthenticated
run_case "delete-entry-git-visible" case_delete_entry_git_visible
run_case "move-entry-git-visible" case_move_entry_git_visible
run_case "tags-list-create-delete" case_tags_list_create_delete
run_case "delete-entry-unauth-401" case_delete_entry_unauth_401

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
