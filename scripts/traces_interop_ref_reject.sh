#!/usr/bin/env bash
# Real ref deletion and non-fast-forward rejection driver (MTR-64).
#
# Run through the dispatcher:
#   MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case ref-reject
#
# Required fixture (the same authorized owner for both transports):
#   MEGA2_TRACES_INTEROP_HTTP_URL   mega2 origin
#   MEGA2_TRACES_INTEROP_TARGET     canonical project path
#   MEGA2_TRACES_INTEROP_TOKEN_FILE owner HTTP token
#   MEGA2_TRACES_INTEROP_SSH_URL    ssh origin, ssh://git@host:port/project/<name>.git
#   MEGA2_TRACES_INTEROP_AGENT_REPO prepared Libra repository (epoch bound)
#   MEGA2_TRACES_INTEROP_SCOPE      stream UUID of that prepared scope
#   LIBRA_SSH_COMMAND               executable ssh wrapper with the bound key
#
# Every case first establishes a successful push baseline (AC-6), then sends
# the deletion or a forced non-fast-forward update to the same by-scope
# locator. A rejection must be the ref policy (the advertisement offers no
# `delete-refs`; a forced update answers 409 with a ref-policy error), never an
# authentication failure or an old-tip CAS failure, and the remote tip must be
# unchanged afterwards (AC-5).
set -euo pipefail

LIBRA_BIN="${LIBRA_BIN:-libra}"
GIT_BIN="${GIT_BIN:-git}"
failures=0
executed=0
passed=0

fail_fixture() {
  echo "FAIL: fixture $1"
}

for var in MEGA2_TRACES_INTEROP_HTTP_URL \
           MEGA2_TRACES_INTEROP_TARGET \
           MEGA2_TRACES_INTEROP_TOKEN_FILE \
           MEGA2_TRACES_INTEROP_SSH_URL \
           MEGA2_TRACES_INTEROP_AGENT_REPO \
           MEGA2_TRACES_INTEROP_SCOPE \
           LIBRA_SSH_COMMAND; do
  if [ -z "${!var:-}" ]; then
    fail_fixture "$var is required"
    exit 1
  fi
done
[ -r "$MEGA2_TRACES_INTEROP_TOKEN_FILE" ] || {
  fail_fixture "MEGA2_TRACES_INTEROP_TOKEN_FILE must be readable"
  exit 1
}
[ -d "$MEGA2_TRACES_INTEROP_AGENT_REPO" ] || {
  fail_fixture "MEGA2_TRACES_INTEROP_AGENT_REPO must name the prepared Libra repository"
  exit 1
}
[ -x "$LIBRA_SSH_COMMAND" ] || {
  fail_fixture "LIBRA_SSH_COMMAND must name an executable ssh wrapper"
  exit 1
}
command -v "$LIBRA_BIN" >/dev/null 2>&1 || {
  fail_fixture "Libra binary '$LIBRA_BIN' is not runnable"
  exit 1
}
command -v "$GIT_BIN" >/dev/null 2>&1 || {
  fail_fixture "Git binary '$GIT_BIN' is not runnable"
  exit 1
}

BASE_URL="${MEGA2_TRACES_INTEROP_HTTP_URL%/}"
SSH_URL="${MEGA2_TRACES_INTEROP_SSH_URL%/}"
TOKEN="$(cat "$MEGA2_TRACES_INTEROP_TOKEN_FILE")"
SCOPE="$MEGA2_TRACES_INTEROP_SCOPE"
HTTP_LOCATOR="$BASE_URL/.traces/by-scope/${SCOPE}.git"
AUTHORITY="${SSH_URL#ssh://}"
AUTHORITY="${AUTHORITY%%/*}"
SSH_HOST="${AUTHORITY##*@}"
SSH_HOST="${SSH_HOST%%:*}"
SSH_PORT="${AUTHORITY##*:}"
case "$SSH_PORT" in ''|*[!0-9]*) SSH_PORT=2222 ;; esac
SSH_LOCATOR="ssh://traces@${SSH_HOST}:${SSH_PORT}/.traces/by-scope/${SCOPE}.git"

OWN_WORKDIR="${MEGA2_TRACES_INTEROP_WORKDIR:-}"
if [ -z "$OWN_WORKDIR" ]; then
  OWN_WORKDIR="$(mktemp -d)"
  OWNED_WORKDIR=1
else
  mkdir -p "$OWN_WORKDIR"
  OWNED_WORKDIR=0
fi
echo "traces interop workdir: $OWN_WORKDIR"
cleanup() {
  if [ "$OWNED_WORKDIR" -eq 1 ] && [ "${MEGA2_TRACES_INTEROP_KEEP_WORKDIR:-}" != "1" ]; then
    rm -rf "$OWN_WORKDIR"
  else
    echo "traces interop workdir kept at: $OWN_WORKDIR"
  fi
}
trap cleanup EXIT

run_case() {
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

git_http() {
  "$GIT_BIN" -c credential.helper= -c "http.extraHeader=Authorization: Bearer ${TOKEN}" "$@"
}

git_ssh() {
  GIT_SSH_COMMAND="$LIBRA_SSH_COMMAND" GIT_TERMINAL_PROMPT=0 "$GIT_BIN" "$@"
}

# Establish the authorized baseline: a successful scoped push and its tip.
baseline() {
  local repo="$OWN_WORKDIR/baseline-repo" locator="$1" transport="$2" tip
  rm -rf "$repo"
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  if [ "$transport" = "http" ]; then
    ( cd "$repo" && "$LIBRA_BIN" agent push --mega2 --remote origin \
        --target "$MEGA2_TRACES_INTEROP_TARGET" \
        --token-file "$MEGA2_TRACES_INTEROP_TOKEN_FILE" ) >"$OWN_WORKDIR/baseline-${transport}.log" 2>&1 || return 1
    tip="$(git_http ls-remote "$locator" refs/libra/traces | awk '{print $1}')"
  else
    ( cd "$repo" &&
      "$LIBRA_BIN" remote set-url origin "$MEGA2_TRACES_INTEROP_SSH_URL" >/dev/null 2>&1 &&
      LIBRA_SSH_COMMAND="$LIBRA_SSH_COMMAND" "$LIBRA_BIN" agent push --mega2 --remote origin \
        --target "$MEGA2_TRACES_INTEROP_TARGET" ) >"$OWN_WORKDIR/baseline-${transport}.log" 2>&1 || return 1
    tip="$(git_ssh ls-remote "$locator" refs/libra/traces | awk '{print $1}')"
  fi
  [ -n "$tip" ] || return 1
  printf '%s' "$tip"
}

# Current tip, for the post-rejection comparison.
current_tip() {
  local locator="$1" transport="$2"
  if [ "$transport" = "http" ]; then
    git_http ls-remote "$locator" refs/libra/traces | awk '{print $1}'
  else
    git_ssh ls-remote "$locator" refs/libra/traces | awk '{print $1}'
  fi
}

# The advertisement must not offer `delete-refs`: that is the ref-policy
# rejection for deletions rather than an authentication or CAS failure.
delete_capability_absent() {
  local locator="$1" transport="$2" trace="$OWN_WORKDIR/caps-${transport}.log"
  if [ "$transport" = "http" ]; then
    GIT_TRACE_PACKET=1 git_http ls-remote "$locator" >/dev/null 2>"$trace" || true
  else
    GIT_TRACE_PACKET=1 git_ssh ls-remote "$locator" >/dev/null 2>"$trace" || true
  fi
  if grep -aq "delete-refs" "$trace"; then
    echo "the advertisement offers delete-refs" >&2
    return 1
  fi
  grep -aq "refs/libra/traces" "$trace"
}

# AC-1/AC-2: deletion is rejected by ref policy and the tip is unchanged.
delete_case() {
  local transport="$1" locator tip after
  if [ "$transport" = "http" ]; then locator="$HTTP_LOCATOR"; else locator="$SSH_LOCATOR"; fi
  tip="$(baseline "$locator" "$transport")" || return 1
  if [ "$transport" = "http" ]; then
    GIT_TRACE_CURL=1 git_http push --no-thin "$locator" :refs/libra/traces \
      >"$OWN_WORKDIR/delete-${transport}.log" 2>&1 && return 1
  else
    git_ssh push --no-thin "$locator" :refs/libra/traces \
      >"$OWN_WORKDIR/delete-${transport}.log" 2>&1 && return 1
  fi
  grep -aq "refs/libra/traces" "$OWN_WORKDIR/delete-${transport}.log" || return 1
  grep -aqE "HTTP/1.1 401|HTTP/1.1 403|unauthenticated" "$OWN_WORKDIR/delete-${transport}.log" && return 1
  delete_capability_absent "$locator" "$transport" || return 1
  after="$(current_tip "$locator" "$transport")"
  [ "$after" = "$tip" ]
}

# AC-3/AC-4: a forced non-fast-forward update answers 409 with a ref-policy
# error and the tip is unchanged.
non_ff_case() {
  local transport="$1" locator tip after repo tree commit
  if [ "$transport" = "http" ]; then locator="$HTTP_LOCATOR"; else locator="$SSH_LOCATOR"; fi
  tip="$(baseline "$locator" "$transport")" || return 1
  repo="$OWN_WORKDIR/nonff-${transport}"
  rm -rf "$repo"
  mkdir -p "$repo"
  (
    cd "$repo"
    "$GIT_BIN" init -q .
    tree="$("$GIT_BIN" write-tree)"
    commit="$(printf 'non-fast-forward sentinel\n' | \
      GIT_AUTHOR_NAME=interop GIT_AUTHOR_EMAIL=interop@example.invalid \
      GIT_COMMITTER_NAME=interop GIT_COMMITTER_EMAIL=interop@example.invalid \
      "$GIT_BIN" commit-tree "$tree")"
    if [ "$transport" = "http" ]; then
      GIT_TRACE_CURL=1 git_http push --no-thin "$locator" "+$commit:refs/libra/traces"
    else
      git_ssh push --no-thin "$locator" "+$commit:refs/libra/traces"
    fi
  ) >"$OWN_WORKDIR/nonff-${transport}.log" 2>&1 && return 1
  grep -aqE "HTTP/1.1 401|HTTP/1.1 403|unauthenticated" "$OWN_WORKDIR/nonff-${transport}.log" && return 1
  if [ "$transport" = "http" ]; then
    grep -aq "HTTP/1.1 409" "$OWN_WORKDIR/nonff-${transport}.log" || return 1
  fi
  grep -aq "object_integr" "$OWN_WORKDIR/nonff-${transport}.log" || return 1
  after="$(current_tip "$locator" "$transport")"
  [ "$after" = "$tip" ]
}

run_case http_delete delete_case http
run_case ssh_delete delete_case ssh
run_case http_non_ff non_ff_case http
run_case ssh_non_ff non_ff_case ssh

echo "traces interop ref-reject summary: executed=$executed passed=$passed failed=$failures"
[ "$failures" -eq 0 ] && [ "$executed" -gt 0 ]
