#!/usr/bin/env bash
# SSH traces client ↔ mega2 interop driver (MTR-59).
#
# Run through the dispatcher:
#   MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case ssh
#
# Required fixture:
#   MEGA2_TRACES_INTEROP_SSH_URL       ssh origin, e.g. ssh://git@127.0.0.1:2222/project/interop.git
#   MEGA2_TRACES_INTEROP_TARGET        canonical project path owning the traces
#   MEGA2_TRACES_INTEROP_AGENT_REPO    a Libra repository whose `traces` branch is ready
#   MEGA2_TRACES_INTEROP_SCOPE         stream UUID of the prepared scope
#   MEGA2_TRACES_INTEROP_FOREIGN_WANT  a commit OID that lives in another scope
#   LIBRA_SSH_COMMAND                  executable wrapper that runs ssh with the bound key
#
# Optional: LIBRA_BIN, GIT_BIN, MEGA2_TRACES_INTEROP_WORKDIR,
# MEGA2_TRACES_INTEROP_KEEP_WORKDIR (same semantics as the HTTP driver).
#
# Output contract: ==> <case> / PASS: <case> / FAIL: <case> and a final
# `traces interop ssh summary: executed=<n> passed=<n> failed=<n>` line.
set -euo pipefail

LIBRA_BIN="${LIBRA_BIN:-libra}"
GIT_BIN="${GIT_BIN:-git}"
SSH_PORT_DEFAULT=2222
failures=0
executed=0
passed=0

fail_fixture() {
  echo "FAIL: fixture $1"
}

if [ -z "${MEGA2_TRACES_INTEROP_SSH_URL:-}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_SSH_URL is required"
  exit 1
fi
if [ -z "${MEGA2_TRACES_INTEROP_TARGET:-}" ]; then
  fail_fixture "MEGA2_TRACES_INTEROP_TARGET is required"
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
if [ -z "${LIBRA_SSH_COMMAND:-}" ] || [ ! -x "${LIBRA_SSH_COMMAND}" ]; then
  fail_fixture "LIBRA_SSH_COMMAND must name an executable ssh wrapper"
  exit 1
fi
command -v "$LIBRA_BIN" >/dev/null 2>&1 || {
  fail_fixture "Libra binary '$LIBRA_BIN' is not runnable"
  exit 1
}
command -v "$GIT_BIN" >/dev/null 2>&1 || {
  fail_fixture "Git binary '$GIT_BIN' is not runnable"
  exit 1
}

SSH_URL="${MEGA2_TRACES_INTEROP_SSH_URL%/}"
AUTHORITY="${SSH_URL#ssh://}"
AUTHORITY="${AUTHORITY%%/*}"
SSH_HOST="${AUTHORITY##*@}"
SSH_HOST="${SSH_HOST%%:*}"
SSH_PORT="${AUTHORITY##*:}"
case "$SSH_PORT" in
  ''|*[!0-9]*) SSH_PORT="$SSH_PORT_DEFAULT" ;;
esac
SCOPE_LOCATOR="$SSH_URL.not-a-locator"
SCOPE_LOCATOR="ssh://traces@${SSH_HOST}:${SSH_PORT}/.traces/by-scope/${MEGA2_TRACES_INTEROP_SCOPE}.git"

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

# AC-1: authenticated scoped push over SSH.
case_push() {
  local repo="$OWN_WORKDIR/push-repo"
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo" &&
      LIBRA_SSH_COMMAND="$LIBRA_SSH_COMMAND" "$LIBRA_BIN" agent push --mega2 \
        --remote origin \
        --target "$MEGA2_TRACES_INTEROP_TARGET"
  )
}

# AC-2: explicit fetch over SSH.
case_fetch() {
  local repo="$OWN_WORKDIR/fetch-repo"
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo" &&
      LIBRA_SSH_COMMAND="$LIBRA_SSH_COMMAND" "$LIBRA_BIN" mega2 traces fetch \
        --remote origin \
        --stream "$MEGA2_TRACES_INTEROP_SCOPE"
  )
}

# AC-3: none-auth is rejected at the reserved `traces` login.
case_none_auth() {
  local output status
  set +e
  output="$(LIBRA_SSH_COMMAND="$LIBRA_SSH_COMMAND" \
    ssh -p "$SSH_PORT" -o BatchMode=yes -o PreferredAuthentications=none \
      -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      "traces@${SSH_HOST}" true 2>&1)"
  status=$?
  set -e
  [ "$status" -ne 0 ] || {
    echo "none-auth unexpectedly succeeded" >&2
    return 1
  }
  case "$output" in
    *"Permission denied"*|*"no supported authentication"*) return 0 ;;
    *) echo "none-auth rejection is not a permission denial: $output" >&2; return 1 ;;
  esac
}

# AC-4: asking this scope for another scope's object over SSH is rejected.
case_cross_scope_want() {
  local repo="$OWN_WORKDIR/want-repo"
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo" &&
      GIT_SSH_COMMAND="$LIBRA_SSH_COMMAND" GIT_TERMINAL_PROMPT=0 "$GIT_BIN" \
        fetch --no-tags "$SCOPE_LOCATOR" "$MEGA2_TRACES_INTEROP_FOREIGN_WANT"
  ) >"$OWN_WORKDIR/want.out" 2>"$OWN_WORKDIR/want.err" && return 1
  return 0
}

# AC-5: a push whose delta base lives in another scope is rejected over SSH.
case_cross_scope_delta() {
  local repo="$OWN_WORKDIR/delta-repo" tree commit
  cp -R "$MEGA2_TRACES_INTEROP_AGENT_REPO" "$repo"
  (
    cd "$repo"
    tree="$("$GIT_BIN" rev-parse "HEAD^{tree}")" || exit 1
    commit="$(printf 'cross-scope ssh delta\n' | \
      GIT_AUTHOR_NAME=interop GIT_AUTHOR_EMAIL=interop@example.invalid \
      GIT_COMMITTER_NAME=interop GIT_COMMITTER_EMAIL=interop@example.invalid \
      "$GIT_BIN" commit-tree "$tree" -p "$MEGA2_TRACES_INTEROP_FOREIGN_WANT")" || exit 1
    GIT_SSH_COMMAND="$LIBRA_SSH_COMMAND" GIT_TERMINAL_PROMPT=0 "$GIT_BIN" \
      push --no-thin "$SCOPE_LOCATOR" "$commit:refs/libra/traces"
  ) >"$OWN_WORKDIR/delta.out" 2>"$OWN_WORKDIR/delta.err" && return 1
  return 0
}

run_case ssh_push case_push
run_case ssh_fetch case_fetch
run_case ssh_none_auth case_none_auth
run_case ssh_cross_scope_want case_cross_scope_want
run_case ssh_cross_scope_delta case_cross_scope_delta

echo "traces interop ssh summary: executed=$executed passed=$passed failed=$failures"
[ "$failures" -eq 0 ] && [ "$executed" -gt 0 ]
