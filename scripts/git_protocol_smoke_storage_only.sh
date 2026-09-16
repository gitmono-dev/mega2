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
  MEGA2_HTTP_REPO_URL       Smart HTTP URL (required once HTTP cases exist)
  MEGA2_SSH_REPO_URL        SSH URL for SSH cases
  MEGA2_SMOKE_CASE          Exact case name; run only that case (unmatched → exit 2)
  MEGA2_GIT_SMOKE_PUSH      Set to 1 to enable write cases (when registered)
  MEGA2_GIT_SMOKE_LFS       Set to 1 to enable LFS cases (when registered)
  MEGA2_IT_SEED_TOKEN      Push token for Basic auth when URL has no userinfo
  MEGA2_GIT_SMOKE_WORKDIR    Existing directory for temporary clones
  MEGA2_GIT_SMOKE_KEEP_WORKDIR  Set to 1 to keep temporary clones

Compose black-box example (after service init — see docs/deploy-trunk.md):
  docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
    exec -T -e MEGA2_HTTP_REPO_URL=http://mega2:8000/ \
    -e MEGA2_SMOKE_CASE='HTTP ls-remote' \
    git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh

Persist host log (ADR-SO-10):
  mkdir -p target/tmp
  LOG="target/tmp/so-smoke-$(date -u +%Y%m%dT%H%M%SZ)-${MEGA2_SMOKE_CASE:-all}.log"
  set -o pipefail
  docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
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
  if [[ -z "${MEGA2_HTTP_REPO_URL:-}" ]]; then
    echo "MEGA2_HTTP_REPO_URL is required for HTTP smoke cases" >&2
    return 1
  fi
}

# Trunk SSH reads use auth_none when anonymous_access=true (no UserStorage key).
ensure_git_ssh_command() {
  if [[ -n "${GIT_SSH_COMMAND:-}" ]]; then
    return 0
  fi
  local kh="${ROOT_DIR}/ssh-known_hosts"
  # accept-new: first-contact host key only; does not disable verification afterward.
  export GIT_SSH_COMMAND="ssh -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=${kh}"
}

require_ssh_url() {
  if [[ -z "${MEGA2_SSH_REPO_URL:-}" ]]; then
    echo "MEGA2_SSH_REPO_URL is required for SSH smoke cases" >&2
    return 1
  fi
  ensure_git_ssh_command
}

case_http_ls_remote() {
  require_http_url || return 1
  git_case ls-remote "$MEGA2_HTTP_REPO_URL"
}

clone_case() {
  local url="$1"
  local dest="$2"
  shift 2
  rm -rf "$dest"
  # Protocol clone/fetch cases are not LFS smudge gates; tip may contain LFS
  # pointers whose PUBLIC_BASE_URL is host-facing (ADR-SO-04).
  GIT_LFS_SKIP_SMUDGE=1 git_case clone "$@" "$url" "$dest" || return
  git -C "$dest" fsck --no-dangling >/dev/null || return
}

case_http_clone() {
  require_http_url || return 1
  clone_case "$MEGA2_HTTP_REPO_URL" "$ROOT_DIR/http-clone"
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
  clone_case "$MEGA2_HTTP_REPO_URL" "$dest" || return
  fetch_case "$dest"
}

case_http_protocol_v2_fetch() {
  require_http_url || return 1
  local dest="$ROOT_DIR/http-v2-fetch"
  clone_case "$MEGA2_HTTP_REPO_URL" "$dest" || return
  fetch_case "$dest" -c protocol.version=2
}

case_http_shallow_clone() {
  require_http_url || return 1
  local dest="$ROOT_DIR/http-shallow"
  clone_case "$MEGA2_HTTP_REPO_URL" "$dest" --depth=1 || return
  local shallow
  shallow="$(git -C "$dest" rev-parse --is-shallow-repository)" || return
  if [[ "$shallow" != "true" ]]; then
    echo "FAIL: expected shallow repository after --depth=1 clone, got is-shallow=$shallow" >&2
    return 1
  fi
}

case_http_protocol_v2_ls_remote() {
  require_http_url || return 1
  git_case -c protocol.version=2 ls-remote "$MEGA2_HTTP_REPO_URL"
}

clone_protocol_v2_blobless() {
  local url="$1"
  local dest="$2"
  local log="$3"
  local filter
  rm -rf "$dest"
  if ! GIT_LFS_SKIP_SMUDGE=1 git_case -c protocol.version=2 clone --filter=blob:none "$url" "$dest" 2>"$log"; then
    cat "$log" >&2
    return 1
  fi
  if grep -qi "filtering not recognized" "$log"; then
    cat "$log" >&2
    return 1
  fi
  git -C "$dest" fsck --no-dangling >/dev/null || return
  filter="$(git -C "$dest" config --get remote.origin.partialclonefilter || true)"
  if [[ "$filter" != "blob:none" ]]; then
    echo "FAIL: expected remote.origin.partialclonefilter=blob:none, got '${filter:-<empty>}'" >&2
    return 1
  fi
}

case_http_protocol_v2_blob_none_clone() {
  require_http_url || return 1
  clone_protocol_v2_blobless \
    "$MEGA2_HTTP_REPO_URL" \
    "$ROOT_DIR/http-blobless" \
    "$ROOT_DIR/http-blobless.stderr"
}

# Inject Basic credentials for trunk receive-pack (ADR-SO-03). Username is ignored.
http_url_with_push_token() {
  local url="${1:-}"
  local token="${2:-}"
  if [[ -z "$url" ]]; then
    echo "http_url_with_push_token: empty URL" >&2
    return 1
  fi
  # Already has userinfo.
  if [[ "$url" =~ ^https?://[^/@]+:[^/@]+@ ]]; then
    printf '%s\n' "$url"
    return 0
  fi
  if [[ -z "$token" ]]; then
    echo "MEGA2_IT_SEED_TOKEN (or credentialed MEGA2_HTTP_REPO_URL) is required for trunk push" >&2
    return 1
  fi
  case "$url" in
    http://*) printf 'http://x:%s@%s\n' "$token" "${url#http://}" ;;
    https://*) printf 'https://x:%s@%s\n' "$token" "${url#https://}" ;;
    *)
      echo "unsupported HTTP URL scheme: $url" >&2
      return 1
      ;;
  esac
}

remote_tip_sha() {
  local url="$1"
  git_case ls-remote "$url" HEAD | awk '{print $1; exit}'
}

# Trunk write: tip must advance (ADR-SO-02). CL-ref creation is not a pass criterion.
case_http_trunk_push() {
  require_http_url || return 1
  if [[ "${MEGA2_GIT_SMOKE_PUSH:-}" != "1" ]]; then
    echo "MEGA2_GIT_SMOKE_PUSH=1 is required for HTTP trunk push" >&2
    return 1
  fi
  local auth_url src before after head
  # B0 rejects path=/; default stack exposes /project after service init (deploy-trunk §9).
  local repo_url
  repo_url="$(trunk_http_repo_url)" || return 1
  auth_url="$(http_url_with_push_token "$repo_url" "${MEGA2_IT_SEED_TOKEN:-}")" || return 1
  src="$ROOT_DIR/http-trunk-push"
  rm -rf "$src"
  before="$(remote_tip_sha "$auth_url")" || return 1
  if [[ -z "$before" ]]; then
    echo "FAIL: could not resolve remote HEAD tip before push" >&2
    return 1
  fi
  git_case clone "$auth_url" "$src" >/dev/null || return 1
  git -C "$src" config user.name "Mega2 Smoke" || return 1
  git -C "$src" config user.email "mega2-smoke@example.invalid" || return 1
  printf 'mega2 trunk smoke %s\n' "$(date -u +%Y%m%dT%H%M%SZ)-$$" >"$src/trunk-smoke.txt"
  git -C "$src" add trunk-smoke.txt || return 1
  git -C "$src" commit -m "mega2 trunk smoke" >/dev/null || return 1
  head="$(git -C "$src" rev-parse HEAD)" || return 1
  git_case -C "$src" -c pack.window=0 -c pack.depth=0 push origin "HEAD:refs/heads/main" || return 1
  after="$(remote_tip_sha "$auth_url")" || return 1
  if [[ -z "$after" || "$after" == "$before" ]]; then
    echo "FAIL: trunk tip did not advance (before=$before after=${after:-<empty>})" >&2
    return 1
  fi
  if [[ "$after" != "$head" ]]; then
    echo "FAIL: N=1 trunk tip must equal client HEAD (expected=$head got=$after)" >&2
    return 1
  fi
}

trunk_http_repo_url() {
  local repo_url="${MEGA2_HTTP_REPO_URL:-}"
  require_http_url || return 1
  if [[ "$repo_url" =~ ^https?://[^/]+/?$ ]]; then
    repo_url="${repo_url%/}/project"
  fi
  printf '%s\n' "$repo_url"
}

# Tag push must fail; remote must not retain the tag (ADR-SO-02).
case_http_reject_git_client_tag_push() {
  if [[ "${MEGA2_GIT_SMOKE_PUSH:-}" != "1" ]]; then
    echo "MEGA2_GIT_SMOKE_PUSH=1 is required for HTTP reject Git-client tag push" >&2
    return 1
  fi
  local repo_url auth_url src tag push_status=0
  repo_url="$(trunk_http_repo_url)" || return 1
  auth_url="$(http_url_with_push_token "$repo_url" "${MEGA2_IT_SEED_TOKEN:-}")" || return 1
  src="$ROOT_DIR/http-reject-tag"
  tag="mega2-smoke-tag-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  rm -rf "$src"
  git_case clone "$auth_url" "$src" >/dev/null || return 1
  git -C "$src" config user.name "Mega2 Smoke" || return 1
  git -C "$src" config user.email "mega2-smoke@example.invalid" || return 1
  printf 'mega2 git tag smoke %s\n' "$tag" >"$src/smoke-tag.txt"
  git -C "$src" add smoke-tag.txt || return 1
  git -C "$src" commit -m "mega2 git tag smoke" >/dev/null || return 1
  git -C "$src" tag "$tag" || return 1
  set +e
  git_case -C "$src" -c pack.window=0 -c pack.depth=0 push origin "refs/tags/$tag"
  push_status=$?
  set -e
  if [[ "$push_status" -eq 0 ]]; then
    echo "FAIL: trunk must reject Git-client tag push" >&2
    git -C "$src" push origin ":refs/tags/$tag" >/dev/null 2>&1 || true
    return 1
  fi
  if git_case ls-remote "$auth_url" "refs/tags/$tag" | rg -q .; then
    echo "FAIL: rejected tag push must not leave refs/tags/$tag on remote" >&2
    return 1
  fi
}

# Unauthenticated receive-pack must fail under push_auth=token (ADR-SO-03).
case_http_reject_unauthenticated_push() {
  require_http_url || return 1
  local repo_url src push_status=0 before after
  repo_url="$(trunk_http_repo_url)" || return 1
  # Force no userinfo even if caller passed a credentialed URL.
  case "$repo_url" in
    http://*@*) repo_url="http://${repo_url#*@}" ;;
    https://*@*) repo_url="https://${repo_url#*@}" ;;
  esac
  src="$ROOT_DIR/http-reject-unauth"
  rm -rf "$src"
  before="$(remote_tip_sha "$repo_url")" || return 1
  git_case clone "$repo_url" "$src" >/dev/null || return 1
  git -C "$src" config user.name "Mega2 Smoke" || return 1
  git -C "$src" config user.email "mega2-smoke@example.invalid" || return 1
  printf 'mega2 unauth push smoke %s\n' "$(date -u +%Y%m%dT%H%M%SZ)-$$" >"$src/unauth-smoke.txt"
  git -C "$src" add unauth-smoke.txt || return 1
  git -C "$src" commit -m "mega2 unauth push smoke" >/dev/null || return 1
  set +e
  git_case -C "$src" -c pack.window=0 -c pack.depth=0 push origin "HEAD:refs/heads/main"
  push_status=$?
  set -e
  if [[ "$push_status" -eq 0 ]]; then
    echo "FAIL: unauthenticated trunk push must be rejected under push_auth=token" >&2
    return 1
  fi
  after="$(remote_tip_sha "$repo_url")" || return 1
  if [[ -n "$before" && "$after" != "$before" ]]; then
    echo "FAIL: rejected unauth push must not advance tip (before=$before after=$after)" >&2
    return 1
  fi
}

# Anonymous tip advance under push_auth=none (ADR-SO-06 stack).
case_http_trunk_push_none() {
  require_http_url || return 1
  if [[ "${MEGA2_GIT_SMOKE_PUSH:-}" != "1" ]]; then
    echo "MEGA2_GIT_SMOKE_PUSH=1 is required for HTTP trunk push (none)" >&2
    return 1
  fi
  local repo_url src before after head
  repo_url="$(trunk_http_repo_url)" || return 1
  case "$repo_url" in
    http://*@*) repo_url="http://${repo_url#*@}" ;;
    https://*@*) repo_url="https://${repo_url#*@}" ;;
  esac
  src="$ROOT_DIR/http-trunk-push-none"
  rm -rf "$src"
  before="$(remote_tip_sha "$repo_url")" || return 1
  if [[ -z "$before" ]]; then
    echo "FAIL: could not resolve remote HEAD tip before anonymous push" >&2
    return 1
  fi
  git_case clone "$repo_url" "$src" >/dev/null || return 1
  git -C "$src" config user.name "Mega2 Smoke" || return 1
  git -C "$src" config user.email "mega2-smoke@example.invalid" || return 1
  printf 'mega2 trunk none smoke %s\n' "$(date -u +%Y%m%dT%H%M%SZ)-$$" >"$src/trunk-none-smoke.txt"
  git -C "$src" add trunk-none-smoke.txt || return 1
  git -C "$src" commit -m "mega2 trunk none smoke" >/dev/null || return 1
  head="$(git -C "$src" rev-parse HEAD)" || return 1
  git_case -C "$src" -c pack.window=0 -c pack.depth=0 push origin "HEAD:refs/heads/main" || return 1
  after="$(remote_tip_sha "$repo_url")" || return 1
  if [[ -z "$after" || "$after" == "$before" ]]; then
    echo "FAIL: anonymous trunk tip did not advance (before=$before after=${after:-<empty>})" >&2
    return 1
  fi
  if [[ "$after" != "$head" ]]; then
    echo "FAIL: N=1 anonymous tip must equal client HEAD (expected=$head got=$after)" >&2
    return 1
  fi
}

# Tag push must fail under push_auth=none; remote must not retain the tag.
case_http_reject_git_client_tag_push_none() {
  if [[ "${MEGA2_GIT_SMOKE_PUSH:-}" != "1" ]]; then
    echo "MEGA2_GIT_SMOKE_PUSH=1 is required for HTTP reject Git-client tag push (none)" >&2
    return 1
  fi
  local repo_url src tag push_status=0
  repo_url="$(trunk_http_repo_url)" || return 1
  case "$repo_url" in
    http://*@*) repo_url="http://${repo_url#*@}" ;;
    https://*@*) repo_url="https://${repo_url#*@}" ;;
  esac
  src="$ROOT_DIR/http-reject-tag-none"
  tag="mega2-smoke-tag-none-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  rm -rf "$src"
  git_case clone "$repo_url" "$src" >/dev/null || return 1
  git -C "$src" config user.name "Mega2 Smoke" || return 1
  git -C "$src" config user.email "mega2-smoke@example.invalid" || return 1
  printf 'mega2 git tag none smoke %s\n' "$tag" >"$src/smoke-tag-none.txt"
  git -C "$src" add smoke-tag-none.txt || return 1
  git -C "$src" commit -m "mega2 git tag none smoke" >/dev/null || return 1
  git -C "$src" tag "$tag" || return 1
  set +e
  git_case -C "$src" -c pack.window=0 -c pack.depth=0 push origin "refs/tags/$tag"
  push_status=$?
  set -e
  if [[ "$push_status" -eq 0 ]]; then
    echo "FAIL: none stack must reject Git-client tag push" >&2
    git -C "$src" push origin ":refs/tags/$tag" >/dev/null 2>&1 || true
    return 1
  fi
  if git_case ls-remote "$repo_url" "refs/tags/$tag" | rg -q .; then
    echo "FAIL: rejected tag push must not leave refs/tags/$tag on remote" >&2
    return 1
  fi
}

lfs_http_url_for() {
  local remote_url="${1%/}"
  printf '%s/info/lfs\n' "$remote_url"
}

configure_lfs_http_remote() {
  local repo_dir="$1"
  local remote_url="$2"
  git -C "$repo_dir" config lfs.url "$(lfs_http_url_for "$remote_url")" || return 1
  git -C "$repo_dir" config lfs.locksverify false || return 1
}

# Trunk LFS: push pointer+object to main, peer pull + cmp (ADR-SO-02/04). No CL-ref fetch.
case_http_lfs_push_and_pull_trunk() {
  require_http_url || return 1
  if [[ "${MEGA2_GIT_SMOKE_PUSH:-}" != "1" ]]; then
    echo "MEGA2_GIT_SMOKE_PUSH=1 is required for HTTP LFS push and pull (trunk)" >&2
    return 1
  fi
  if [[ "${MEGA2_GIT_SMOKE_LFS:-}" != "1" ]]; then
    echo "MEGA2_GIT_SMOKE_LFS=1 is required for HTTP LFS push and pull (trunk)" >&2
    return 1
  fi
  git lfs version >/dev/null 2>&1 || {
    echo "git-lfs is required for MEGA2_GIT_SMOKE_LFS=1" >&2
    return 1
  }
  local repo_url auth_url src peer before after head binary
  repo_url="$(trunk_http_repo_url)" || return 1
  auth_url="$(http_url_with_push_token "$repo_url" "${MEGA2_IT_SEED_TOKEN:-}")" || return 1
  src="$ROOT_DIR/http-lfs-trunk-src"
  peer="$ROOT_DIR/http-lfs-trunk-peer"
  binary="mega2-smoke-lfs.bin"
  rm -rf "$src" "$peer"
  before="$(remote_tip_sha "$auth_url")" || return 1
  if [[ -z "$before" ]]; then
    echo "FAIL: could not resolve remote HEAD tip before LFS push" >&2
    return 1
  fi
  git_case clone "$auth_url" "$src" >/dev/null || return 1
  git -C "$src" config user.name "Mega2 Smoke" || return 1
  git -C "$src" config user.email "mega2-smoke@example.invalid" || return 1
  git -C "$src" lfs install --local >/dev/null || return 1
  configure_lfs_http_remote "$src" "$auth_url" || return 1
  git -C "$src" lfs track "*.bin" >/dev/null || return 1
  printf 'mega2 trunk lfs smoke %s\n' "$(date -u +%Y%m%dT%H%M%SZ)-$$" >"$src/$binary"
  git -C "$src" add .gitattributes "$binary" || return 1
  git -C "$src" commit -m "mega2 trunk lfs smoke" >/dev/null || return 1
  head="$(git -C "$src" rev-parse HEAD)" || return 1
  git_case -C "$src" -c pack.window=0 -c pack.depth=0 push origin "HEAD:refs/heads/main" || return 1
  after="$(remote_tip_sha "$auth_url")" || return 1
  if [[ -z "$after" || "$after" == "$before" ]]; then
    echo "FAIL: LFS trunk tip did not advance (before=$before after=${after:-<empty>})" >&2
    return 1
  fi
  if [[ "$after" != "$head" ]]; then
    echo "FAIL: N=1 LFS tip must equal client HEAD (expected=$head got=$after)" >&2
    return 1
  fi
  mkdir -p "$peer"
  git_case -C "$peer" init >/dev/null || return 1
  git -C "$peer" remote add origin "$auth_url" || return 1
  git -C "$peer" lfs install --local >/dev/null || return 1
  configure_lfs_http_remote "$peer" "$auth_url" || return 1
  git_case -C "$peer" fetch origin "refs/heads/main:refs/heads/main" || return 1
  GIT_LFS_SKIP_SMUDGE=1 git -C "$peer" checkout main >/dev/null || return 1
  git -C "$peer" lfs pull || return 1
  if ! cmp -s "$src/$binary" "$peer/$binary"; then
    echo "FAIL: LFS round-trip content mismatch for $binary" >&2
    return 1
  fi
  git -C "$peer" lfs locks >/dev/null || return 1
}

case_ssh_ls_remote() {
  require_ssh_url || return 1
  git_case ls-remote "$MEGA2_SSH_REPO_URL"
}

case_ssh_clone() {
  require_ssh_url || return 1
  clone_case "$MEGA2_SSH_REPO_URL" "$ROOT_DIR/ssh-clone"
}

case_ssh_fetch() {
  require_ssh_url || return 1
  local dest="$ROOT_DIR/ssh-fetch"
  clone_case "$MEGA2_SSH_REPO_URL" "$dest" || return 1
  fetch_case "$dest"
}

case_ssh_protocol_v2_fetch() {
  require_ssh_url || return 1
  local dest="$ROOT_DIR/ssh-v2-fetch"
  clone_case "$MEGA2_SSH_REPO_URL" "$dest" || return 1
  fetch_case "$dest" -c protocol.version=2
}

case_ssh_shallow_clone() {
  require_ssh_url || return 1
  local dest="$ROOT_DIR/ssh-shallow"
  clone_case "$MEGA2_SSH_REPO_URL" "$dest" --depth=1 || return 1
  local shallow
  shallow="$(git -C "$dest" rev-parse --is-shallow-repository)" || return 1
  if [[ "$shallow" != "true" ]]; then
    echo "FAIL: expected shallow repository after --depth=1 SSH clone, got is-shallow=$shallow" >&2
    return 1
  fi
}

case_ssh_protocol_v2_ls_remote() {
  require_ssh_url || return 1
  git_case -c protocol.version=2 ls-remote "$MEGA2_SSH_REPO_URL"
}

case_ssh_protocol_v2_blob_none_clone() {
  require_ssh_url || return 1
  clone_protocol_v2_blobless \
    "$MEGA2_SSH_REPO_URL" \
    "$ROOT_DIR/ssh-blobless" \
    "$ROOT_DIR/ssh-blobless.stderr"
}

# SSH receive-pack must stay disabled under storage-only (ssh_receive_pack=false).
case_ssh_reject_receive_pack() {
  require_ssh_url || return 1
  local src log push_status=0
  src="$ROOT_DIR/ssh-reject-receive-pack"
  log="$ROOT_DIR/ssh-reject-receive-pack.stderr"
  rm -rf "$src"
  GIT_LFS_SKIP_SMUDGE=1 git_case clone "$MEGA2_SSH_REPO_URL" "$src" >/dev/null || return 1
  git -C "$src" config user.name "Mega2 Smoke" || return 1
  git -C "$src" config user.email "mega2-smoke@example.invalid" || return 1
  printf 'mega2 ssh receive-pack reject %s\n' "$(date -u +%Y%m%dT%H%M%SZ)-$$" >"$src/ssh-rp-reject.txt"
  git -C "$src" add ssh-rp-reject.txt || return 1
  git -C "$src" commit -m "mega2 ssh receive-pack reject" >/dev/null || return 1
  set +e
  git_case -C "$src" -c pack.window=0 -c pack.depth=0 push origin "HEAD:refs/heads/main" >"$log" 2>&1
  push_status=$?
  set -e
  if [[ "$push_status" -eq 0 ]]; then
    echo "FAIL: SSH receive-pack must be rejected under ssh_receive_pack=false" >&2
    return 1
  fi
  if ! rg -q 'SSH receive-pack is disabled' "$log"; then
    echo "FAIL: expected stable disable substring 'SSH receive-pack is disabled' in push output:" >&2
    cat "$log" >&2
    return 1
  fi
}

# --- Protocol cases (registered by plan-20260906 scene cards). ---

run_case "HTTP ls-remote" case_http_ls_remote
run_case "HTTP clone" case_http_clone
run_case "HTTP fetch" case_http_fetch
run_case "HTTP protocol v2 fetch" case_http_protocol_v2_fetch
run_case "HTTP shallow clone depth=1" case_http_shallow_clone
run_case "HTTP protocol v2 ls-remote" case_http_protocol_v2_ls_remote
run_case "HTTP protocol v2 blob:none clone" case_http_protocol_v2_blob_none_clone
run_case "HTTP reject unauthenticated push" case_http_reject_unauthenticated_push

if [[ "${MEGA2_GIT_SMOKE_PUSH:-}" == "1" ]]; then
  run_case "HTTP trunk push" case_http_trunk_push
  run_case "HTTP reject Git-client tag push" case_http_reject_git_client_tag_push
  run_case "HTTP trunk push (none)" case_http_trunk_push_none
  run_case "HTTP reject Git-client tag push (none)" case_http_reject_git_client_tag_push_none
  if [[ "${MEGA2_GIT_SMOKE_LFS:-}" == "1" ]]; then
    run_case "HTTP LFS push and pull (trunk)" case_http_lfs_push_and_pull_trunk
  elif [[ -n "$CASE_FILTER" && "$CASE_FILTER" == "HTTP LFS push and pull (trunk)" ]]; then
    echo "FAIL: MEGA2_SMOKE_CASE='$CASE_FILTER' requires MEGA2_GIT_SMOKE_LFS=1" >&2
    echo "git protocol smoke storage_only summary: 0 passed, 1 failed (${SKIP_COUNT} skipped)"
    exit 2
  fi
elif [[ -n "$CASE_FILTER" && (
  "$CASE_FILTER" == "HTTP trunk push" ||
  "$CASE_FILTER" == "HTTP reject Git-client tag push" ||
  "$CASE_FILTER" == "HTTP trunk push (none)" ||
  "$CASE_FILTER" == "HTTP reject Git-client tag push (none)" ||
  "$CASE_FILTER" == "HTTP LFS push and pull (trunk)"
) ]]; then
  echo "FAIL: MEGA2_SMOKE_CASE='$CASE_FILTER' requires MEGA2_GIT_SMOKE_PUSH=1" >&2
  echo "git protocol smoke storage_only summary: 0 passed, 1 failed (${SKIP_COUNT} skipped)"
  exit 2
fi

if [[ -n "${MEGA2_SSH_REPO_URL:-}" ]]; then
  run_case "SSH ls-remote" case_ssh_ls_remote
  run_case "SSH clone" case_ssh_clone
  run_case "SSH fetch" case_ssh_fetch
  run_case "SSH protocol v2 fetch" case_ssh_protocol_v2_fetch
  run_case "SSH shallow clone depth=1" case_ssh_shallow_clone
  run_case "SSH protocol v2 ls-remote" case_ssh_protocol_v2_ls_remote
  run_case "SSH protocol v2 blob:none clone" case_ssh_protocol_v2_blob_none_clone
  run_case "SSH reject receive-pack" case_ssh_reject_receive_pack
else
  # SO-04: unset SSH URL ⇒ SKIP (not failed), even under CASE filter.
  skip_case "SSH reject receive-pack" "MEGA2_SSH_REPO_URL unset"
  if [[ -n "$CASE_FILTER" && (
    "$CASE_FILTER" == "SSH ls-remote" ||
    "$CASE_FILTER" == "SSH clone" ||
    "$CASE_FILTER" == "SSH fetch" ||
    "$CASE_FILTER" == "SSH protocol v2 fetch" ||
    "$CASE_FILTER" == "SSH shallow clone depth=1" ||
    "$CASE_FILTER" == "SSH protocol v2 ls-remote" ||
    "$CASE_FILTER" == "SSH protocol v2 blob:none clone"
  ) ]]; then
    echo "FAIL: MEGA2_SMOKE_CASE='$CASE_FILTER' requires MEGA2_SSH_REPO_URL" >&2
    echo "git protocol smoke storage_only summary: 0 passed, 1 failed (${SKIP_COUNT} skipped)"
    exit 2
  fi
fi

if [[ -n "$CASE_FILTER" && "$CASE_HIT" -eq 0 ]]; then
  echo "FAIL: MEGA2_SMOKE_CASE='$CASE_FILTER' matched no registered case" >&2
  echo "git protocol smoke storage_only summary: 0 passed, 1 failed (${SKIP_COUNT} skipped)"
  exit 2
fi

echo "git protocol smoke storage_only summary: $PASS_COUNT passed, $FAIL_COUNT failed (${SKIP_COUNT} skipped)"
if [[ "$FAIL_COUNT" -gt 0 ]]; then
  exit 1
fi
exit 0
