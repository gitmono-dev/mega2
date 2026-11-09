#!/usr/bin/env bash
# HTTP traces client ↔ mega2 interop driver (MTR-58).
#
# Run through the dispatcher:
#   MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case http
#
# Required fixture (all values are synthetic test data; never put real
# credentials in URLs or logs):
#   MEGA2_TRACES_INTEROP_HTTP_URL      mega2 origin, e.g. http://127.0.0.1:18000
#   MEGA2_TRACES_INTEROP_TARGET        canonical project path owning the traces,
#                                      e.g. /project/traces-interop
#   MEGA2_TRACES_INTEROP_TOKEN_FILE    file holding the HTTP push token (0600)
#   MEGA2_TRACES_INTEROP_AGENT_REPO    a Libra repository whose `traces` branch is
#                                      ready to push (epoch already bound)
#   MEGA2_TRACES_INTEROP_SCOPE         stream UUID of that prepared scope
#   MEGA2_TRACES_INTEROP_FOREIGN_WANT  a commit OID that lives in another scope
#
# Optional:
#   LIBRA_BIN                          Libra binary (default: libra)
#   GIT_BIN                            Git binary (default: git)
#   MEGA2_TRACES_INTEROP_WORKDIR       workdir to own; default: a fresh mktemp
#   MEGA2_TRACES_INTEROP_KEEP_WORKDIR  set to 1 to keep the workdir
#
# Every case is executed and counted. Missing fixture values, a skipped case
# or zero cases all make the driver fail. Output contract:
#   ==> <case> / PASS: <case> / FAIL: <case>
#   traces interop http summary: executed=<n> passed=<n> failed=<n>
set -euo pipefail

LIBRA_BIN="${LIBRA_BIN:-libra}"
GIT_BIN="${GIT_BIN:-git}"
failures=0
executed=0
passed=0

fail_fixture() {
  echo "FAIL: fixture $1"
}

if [ -z "${MEGA2_TRACES_INTEROP_HTTP_URL:-}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_HTTP_URL is required"
  exit 1
fi
if [ -z "${MEGA2_TRACES_INTEROP_TARGET:-}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_TARGET is required"
  exit 1
fi
if [ -z "${MEGA2_TRACES_INTEROP_TOKEN_FILE:-}" ] || [ ! -r "${MEGA2_TRACES_INTEROP_TOKEN_FILE}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_TOKEN_FILE must name a readable token file"
  exit 1
fi
if [ -z "${MEGA2_TRACES_INTEROP_AGENT_REPO:-}" ] || [ ! -d "${MEGA2_TRACES_INTEROP_AGENT_REPO}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_AGENT_REPO must name a prepared Libra repository"
  exit 1
fi
if [ -z "${MEGA2_TRACES_INTEROP_SCOPE:-}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_SCOPE is required"
  exit 1
fi
if [ -z "${MEGA2_TRACES_INTEROP_FOREIGN_WANT:-}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_FOREIGN_WANT is required"
  exit 1
fi
command -v "$LIBRA_BIN" >/dev/null 2>&1 || {
  fail_fixture "Libra binary '$LIBRA_BIN' is not runnable"
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  fail_fixture "curl is not runnable"
  exit 1
}
command -v "$GIT_BIN" >/dev/null 2>&1 || {
  fail_fixture "Git binary '$GIT_BIN' is not runnable"
  exit 1
}

BASE_URL="${MEGA2_TRACES_INTEROP_HTTP_URL%/}"
SCOPE_LOCATOR="$BASE_URL/.traces/by-scope/${MEGA2_TRACES_INTEROP_SCOPE}.git"
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

# AC-1: an authenticated scoped push succeeds. The prepared repository is
# copied so the fixture itself is never mutated.
case_push() {
  local repo="$OWN_WORKDIR/push-repo"
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo" &&
      "$LIBRA_BIN" agent push --mega2 \
        --remote origin \
        --target "$MEGA2_TRACES_INTEROP_TARGET" \
        --token-file "$MEGA2_TRACES_INTEROP_TOKEN_FILE"
  )
}

# AC-2: an explicit fetch of the same stream succeeds through the Libra client.
case_fetch() {
  local repo="$OWN_WORKDIR/fetch-repo"
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo" &&
      "$LIBRA_BIN" mega2 traces fetch \
        --remote origin \
        --stream "$MEGA2_TRACES_INTEROP_SCOPE" \
        --token-file "$MEGA2_TRACES_INTEROP_TOKEN_FILE"
  )
}

# AC-3: a request without credentials is rejected before any advertisement.
# `git` refuses to send an unauthenticated request when terminal prompts are
# disabled, so this case asks the protocol endpoint directly and requires both
# the 401 status and the challenge header.
case_unauthenticated() {
  local status
  status="$(curl -s -o /dev/null -D "$OWN_WORKDIR/unauth.headers" -w '%{http_code}' \
    "$SCOPE_LOCATOR/info/refs?service=git-receive-pack")"
  [ "$status" = "401" ] || {
    echo "unauthenticated request returned HTTP $status, expected 401" >&2
    return 1
  }
  grep -qi '^www-authenticate: *basic' "$OWN_WORKDIR/unauth.headers" || {
    echo "unauthenticated response has no Basic challenge" >&2
    return 1
  }
}

# AC-4: asking this scope for another scope's object is rejected.
case_cross_scope_want() {
  local repo="$OWN_WORKDIR/want-repo"
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo" &&
      GIT_TERMINAL_PROMPT=0 "$GIT_BIN" \
        -c "credential.helper=" \
        -c "http.extraHeader=Authorization: Bearer $(cat "$MEGA2_TRACES_INTEROP_TOKEN_FILE")" \
        fetch --no-tags "$SCOPE_LOCATOR" "$MEGA2_TRACES_INTEROP_FOREIGN_WANT"
  ) >"$OWN_WORKDIR/want.out" 2>"$OWN_WORKDIR/want.err" && return 1
  return 0
}

# AC-5: a push whose delta base lives in another scope is rejected as a whole.
case_cross_scope_delta() {
  local repo="$OWN_WORKDIR/delta-repo" tree commit tip
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo"
    tree="$("$GIT_BIN" rev-parse "HEAD^{tree}")" || exit 1
    commit="$(printf 'cross-scope delta\n' | \
      GIT_AUTHOR_NAME=interop GIT_AUTHOR_EMAIL=interop@example.invalid \
      GIT_COMMITTER_NAME=interop GIT_COMMITTER_EMAIL=interop@example.invalid \
      "$GIT_BIN" commit-tree "$tree" -p "$MEGA2_TRACES_INTEROP_FOREIGN_WANT")" || exit 1
    tip="$("$GIT_BIN" rev-parse refs/libra/traces)" || exit 1
    GIT_TERMINAL_PROMPT=0 "$GIT_BIN" \
      -c "credential.helper=" \
      -c "http.extraHeader=Authorization: Bearer $(cat "$MEGA2_TRACES_INTEROP_TOKEN_FILE")" \
      push --no-thin "$BASE_URL/.traces/by-scope/${MEGA2_TRACES_INTEROP_SCOPE}.git" \
      "$commit:refs/libra/traces"
  ) >"$OWN_WORKDIR/delta.out" 2>"$OWN_WORKDIR/delta.err" && return 1
  return 0
}

run_case http_push case_push
run_case http_fetch case_fetch
run_case http_unauthenticated case_unauthenticated
run_case http_cross_scope_want case_cross_scope_want
run_case http_cross_scope_delta case_cross_scope_delta

echo "traces interop http summary: executed=$executed passed=$passed failed=$failures"
[ "$failures" -eq 0 ] && [ "$executed" -gt 0 ]
