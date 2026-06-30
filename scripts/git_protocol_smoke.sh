#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Git protocol smoke matrix for a running monoengine service.

Required environment:
  MONOENGINE_HTTP_REPO_URL    Smart HTTP repo URL, e.g. http://127.0.0.1:9000/group/repo.git

Optional environment:
  MONOENGINE_SSH_REPO_URL     SSH repo URL, e.g. ssh://git@127.0.0.1:2222/group/repo.git
  MONOENGINE_GIT_SMOKE_PUSH   Set to 1 to run opt-in HTTP/SSH push/delete smoke
  MONOENGINE_GIT_SMOKE_WORKDIR  Existing directory for temporary clones
  MONOENGINE_GIT_SMOKE_KEEP_WORKDIR  Set to 1 to keep temporary clones after the run

Examples:
  MONOENGINE_HTTP_REPO_URL=http://127.0.0.1:9000/test/project.git \
    bash scripts/git_protocol_smoke.sh

  MONOENGINE_HTTP_REPO_URL=http://127.0.0.1:9000/test/project.git \
  MONOENGINE_SSH_REPO_URL=ssh://git@127.0.0.1:2222/test/project.git \
    bash scripts/git_protocol_smoke.sh
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ -z "${MONOENGINE_HTTP_REPO_URL:-}" ]]; then
  usage >&2
  exit 2
fi

command -v git >/dev/null 2>&1 || {
  echo "git is required" >&2
  exit 2
}

ROOT_DIR="${MONOENGINE_GIT_SMOKE_WORKDIR:-$(mktemp -d)}"
if [[ ! -d "$ROOT_DIR" ]]; then
  mkdir -p "$ROOT_DIR"
fi

cleanup() {
  if [[ -z "${MONOENGINE_GIT_SMOKE_WORKDIR:-}" && "${MONOENGINE_GIT_SMOKE_KEEP_WORKDIR:-}" != "1" ]]; then
    rm -rf "$ROOT_DIR"
  else
    echo "smoke workdir kept at: $ROOT_DIR"
  fi
}
trap cleanup EXIT

PASS_COUNT=0
FAIL_COUNT=0

run_case() {
  local name="$1"
  shift
  echo "==> $name"
  if "$@"; then
    echo "PASS: $name"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "FAIL: $name" >&2
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

git_case() {
  git -c advice.detachedHead=false "$@"
}

clone_case() {
  local url="$1"
  local dest="$2"
  shift 2
  rm -rf "$dest"
  git_case clone "$@" "$url" "$dest" || return
  git -C "$dest" fsck --no-dangling >/dev/null || return
}

clone_protocol_v2_blobless() {
  local url="$1"
  local dest="$2"
  local log="$3"
  rm -rf "$dest"
  if ! git_case -c protocol.version=2 clone --filter=blob:none "$url" "$dest" 2>"$log"; then
    cat "$log" >&2
    return 1
  fi
  if grep -qi "filtering not recognized" "$log"; then
    cat "$log" >&2
    return 1
  fi
  git -C "$dest" fsck --no-dangling >/dev/null || return
}

fetch_case() {
  local dest="$1"
  shift
  git_case -C "$dest" "$@" fetch --all --prune || return
  git -C "$dest" fsck --no-dangling >/dev/null || return
}

push_branch_smoke() {
  local remote_url="$1"
  local label="$2"
  local src="$ROOT_DIR/push-$label-src"
  local branch="monoengine-smoke-$(date +%s)-$$"
  local cleanup_status=0
  rm -rf "$src"
  mkdir -p "$src" || return
  git -C "$src" init >/dev/null || return
  git -C "$src" config user.name "Monoengine Smoke" || return
  git -C "$src" config user.email "monoengine-smoke@example.invalid" || return
  printf 'monoengine git smoke %s\n' "$branch" >"$src/smoke.txt"
  git -C "$src" add smoke.txt || return
  git -C "$src" commit -m "monoengine git smoke" >/dev/null || return
  git -C "$src" remote add origin "$remote_url" || return
  git -C "$src" push origin "HEAD:refs/heads/$branch" || return
  git -C "$src" push origin ":refs/heads/$branch" || cleanup_status=$?
  if [[ "$cleanup_status" -ne 0 ]]; then
    echo "failed to delete remote smoke branch refs/heads/$branch" >&2
    return "$cleanup_status"
  fi
}

push_tag_smoke() {
  local remote_url="$1"
  local label="$2"
  local src="$ROOT_DIR/push-$label-tag-src"
  local tag="monoengine-smoke-tag-$(date +%s)-$$"
  local cleanup_status=0
  rm -rf "$src"
  mkdir -p "$src" || return
  git -C "$src" init >/dev/null || return
  git -C "$src" config user.name "Monoengine Smoke" || return
  git -C "$src" config user.email "monoengine-smoke@example.invalid" || return
  printf 'monoengine git tag smoke %s\n' "$tag" >"$src/smoke-tag.txt"
  git -C "$src" add smoke-tag.txt || return
  git -C "$src" commit -m "monoengine git tag smoke" >/dev/null || return
  git -C "$src" tag "$tag" || return
  git -C "$src" remote add origin "$remote_url" || return
  git -C "$src" push origin "refs/tags/$tag" || return
  git -C "$src" push origin ":refs/tags/$tag" || cleanup_status=$?
  if [[ "$cleanup_status" -ne 0 ]]; then
    echo "failed to delete remote smoke tag refs/tags/$tag" >&2
    return "$cleanup_status"
  fi
}

run_case "HTTP ls-remote" git_case ls-remote "$MONOENGINE_HTTP_REPO_URL"
run_case "HTTP clone" clone_case "$MONOENGINE_HTTP_REPO_URL" "$ROOT_DIR/http-clone"
run_case "HTTP fetch" fetch_case "$ROOT_DIR/http-clone"
run_case "HTTP protocol v2 fetch" fetch_case "$ROOT_DIR/http-clone" -c protocol.version=2
run_case "HTTP shallow clone depth=1" clone_case "$MONOENGINE_HTTP_REPO_URL" "$ROOT_DIR/http-shallow" --depth=1
run_case "HTTP protocol v2 ls-remote" git_case -c protocol.version=2 ls-remote "$MONOENGINE_HTTP_REPO_URL"
run_case "HTTP protocol v2 blob:none clone" clone_protocol_v2_blobless "$MONOENGINE_HTTP_REPO_URL" "$ROOT_DIR/http-blobless" "$ROOT_DIR/http-blobless.stderr"

if [[ -n "${MONOENGINE_SSH_REPO_URL:-}" ]]; then
  run_case "SSH ls-remote" git_case ls-remote "$MONOENGINE_SSH_REPO_URL"
  run_case "SSH clone" clone_case "$MONOENGINE_SSH_REPO_URL" "$ROOT_DIR/ssh-clone"
  run_case "SSH fetch" fetch_case "$ROOT_DIR/ssh-clone"
  run_case "SSH protocol v2 fetch" fetch_case "$ROOT_DIR/ssh-clone" -c protocol.version=2
  run_case "SSH shallow clone depth=1" clone_case "$MONOENGINE_SSH_REPO_URL" "$ROOT_DIR/ssh-shallow" --depth=1
  run_case "SSH protocol v2 ls-remote" git_case -c protocol.version=2 ls-remote "$MONOENGINE_SSH_REPO_URL"
  run_case "SSH protocol v2 blob:none clone" clone_protocol_v2_blobless "$MONOENGINE_SSH_REPO_URL" "$ROOT_DIR/ssh-blobless" "$ROOT_DIR/ssh-blobless.stderr"
fi

if [[ "${MONOENGINE_GIT_SMOKE_PUSH:-}" == "1" ]]; then
  run_case "HTTP push and delete branch" push_branch_smoke "$MONOENGINE_HTTP_REPO_URL" "http"
  run_case "HTTP push and delete tag" push_tag_smoke "$MONOENGINE_HTTP_REPO_URL" "http"
  if [[ -n "${MONOENGINE_SSH_REPO_URL:-}" ]]; then
    run_case "SSH push and delete branch" push_branch_smoke "$MONOENGINE_SSH_REPO_URL" "ssh"
    run_case "SSH push and delete tag" push_tag_smoke "$MONOENGINE_SSH_REPO_URL" "ssh"
  fi
else
  echo "SKIP: HTTP/SSH push-delete branch and tag (set MONOENGINE_GIT_SMOKE_PUSH=1 to enable)"
fi

echo "git protocol smoke summary: $PASS_COUNT passed, $FAIL_COUNT failed"
if [[ "$FAIL_COUNT" -ne 0 ]]; then
  exit 1
fi
