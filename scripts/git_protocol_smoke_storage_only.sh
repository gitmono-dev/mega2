#!/usr/bin/env bash
# Storage-only / trunk Git protocol black-box smoke (compose git-smoke).
# Protocol client toolchain: git / git-lfs / ssh only.
# Do NOT use libra as a protocol client (repo VCS ≠ smoke observer).
set -euo pipefail

usage() {
  cat <<'USAGE'
Storage-only / trunk Git protocol smoke (black-box via git client).

Protocol clients: git, git-lfs, OpenSSH ssh. Do not use libra as a client.

Optional environment:
  MONOENGINE_HTTP_REPO_URL       Smart HTTP URL (required once HTTP cases exist)
  MONOENGINE_SSH_REPO_URL        SSH URL for SSH cases
  MONOENGINE_SMOKE_CASE          Exact case name; run only that case (unmatched → exit 2)
  MONOENGINE_GIT_SMOKE_PUSH      Set to 1 to enable write cases (when registered)
  MONOENGINE_GIT_SMOKE_LFS       Set to 1 to enable LFS cases (when registered)
  MONOENGINE_GIT_SMOKE_WORKDIR    Existing directory for temporary clones
  MONOENGINE_GIT_SMOKE_KEEP_WORKDIR  Set to 1 to keep temporary clones

Compose black-box example (after service init — see docs/deploy-trunk.md):
  docker compose -p monoengine-trunk -f docker-compose-storage-only.yml --profile smoke \
    exec -T -e MONOENGINE_HTTP_REPO_URL=http://monoengine:8000/ \
    -e MONOENGINE_SMOKE_CASE='HTTP ls-remote' \
    git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh

Persist host log (ADR-SO-10):
  mkdir -p target/tmp
  LOG="target/tmp/so-smoke-$(date -u +%Y%m%dT%H%M%SZ)-${MONOENGINE_SMOKE_CASE:-all}.log"
  set -o pipefail
  docker compose -p monoengine-trunk -f docker-compose-storage-only.yml --profile smoke \
    exec -T git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh \
    2>&1 | tee "$LOG"
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

command -v git >/dev/null 2>&1 || {
  echo "git is required (protocol smoke client; do not use libra)" >&2
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
SKIP_COUNT=0
CASE_FILTER="${MONOENGINE_SMOKE_CASE:-}"
CASE_HIT=0

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

skip_case() {
  local name="$1"
  local reason="${2:-skipped}"
  if [[ -n "$CASE_FILTER" && "$name" != "$CASE_FILTER" ]]; then
    return 0
  fi
  CASE_HIT=1
  echo "SKIP: $name ($reason)"
  SKIP_COUNT=$((SKIP_COUNT + 1))
}

git_case() {
  git -c advice.detachedHead=false "$@"
}

require_http_url() {
  if [[ -z "${MONOENGINE_HTTP_REPO_URL:-}" ]]; then
    echo "MONOENGINE_HTTP_REPO_URL is required for HTTP smoke cases" >&2
    return 1
  fi
}

case_http_ls_remote() {
  require_http_url || return 1
  git_case ls-remote "$MONOENGINE_HTTP_REPO_URL"
}

clone_case() {
  local url="$1"
  local dest="$2"
  shift 2
  rm -rf "$dest"
  git_case clone "$@" "$url" "$dest" || return
  git -C "$dest" fsck --no-dangling >/dev/null || return
}

case_http_clone() {
  require_http_url || return 1
  clone_case "$MONOENGINE_HTTP_REPO_URL" "$ROOT_DIR/http-clone"
}

fetch_case() {
  local dest="$1"
  shift
  git_case -C "$dest" "$@" fetch --all --prune || return
  git -C "$dest" fsck --no-dangling >/dev/null || return
}

case_http_fetch() {
  require_http_url || return 1
  local dest="$ROOT_DIR/http-fetch"
  clone_case "$MONOENGINE_HTTP_REPO_URL" "$dest" || return
  fetch_case "$dest"
}

case_http_protocol_v2_fetch() {
  require_http_url || return 1
  local dest="$ROOT_DIR/http-v2-fetch"
  clone_case "$MONOENGINE_HTTP_REPO_URL" "$dest" || return
  fetch_case "$dest" -c protocol.version=2
}

# --- Protocol cases (registered by plan-20260906 scene cards). ---

run_case "HTTP ls-remote" case_http_ls_remote
run_case "HTTP clone" case_http_clone
run_case "HTTP fetch" case_http_fetch
run_case "HTTP protocol v2 fetch" case_http_protocol_v2_fetch

if [[ -n "$CASE_FILTER" && "$CASE_HIT" -eq 0 ]]; then
  echo "FAIL: MONOENGINE_SMOKE_CASE='$CASE_FILTER' matched no registered case" >&2
  echo "git protocol smoke storage_only summary: 0 passed, 1 failed (${SKIP_COUNT} skipped)"
  exit 2
fi

echo "git protocol smoke storage_only summary: $PASS_COUNT passed, $FAIL_COUNT failed (${SKIP_COUNT} skipped)"
if [[ "$FAIL_COUNT" -gt 0 ]]; then
  exit 1
fi
exit 0
